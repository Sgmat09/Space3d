//! Storybook viewer: browse a [`Storybook`], run the selected story live in its own app,
//! tweak knobs, control ticks and capture screenshots. Works natively and on the web (where
//! story viewports need WebGPU or WebGL2).
//!
//! A panicking story is reported in the viewer only when panics unwind (dev builds); release
//! builds with `panic = "abort"` and all web builds abort instead — see
//! [`crate::panics_are_isolated`].

use crate::files::{IoJobs, sanitize_file_name, write_file_with};
use crate::sub_app::{LiveView, panic_message, panics_are_isolated};
use crate::widgets::{fill_drag, fill_slider, panel_frame, panel_title, property, section, visible_scroll_bars};
use space3d_core::{App, Plugin, Stage, SystemCtx, Time};
use space3d_platform::InputPlugin;
use space3d_render::RenderPlugin;
use space3d_stories::{Knob, KnobKind, KnobValue, KnobValues, Story, Storybook};
use space3d_ui::{EguiContext, UiPlugin};
use std::panic::AssertUnwindSafe;
use std::time::Duration;

/// Viewer settings (see [`crate::run_storybook_with`]).
#[derive(Clone, Debug)]
pub struct StorybookConfig {
    /// Window title.
    pub title: String,
    /// Window width (logical px).
    pub width: u32,
    /// Window height (logical px).
    pub height: u32,
    /// Directory for "Capture screenshot" PNGs (native; the web downloads them).
    pub screenshot_dir: String,
    /// Run without a window at this size (CI smoke runs). Also enabled by `SPACE3D_HEADLESS`.
    pub headless: Option<(u32, u32)>,
    /// Quit after this many frames (also `SPACE3D_EXIT_AFTER_FRAMES`).
    pub exit_after_frames: Option<u64>,
    /// Story selected at startup (default: the first one).
    pub initial_story: Option<String>,
}

impl Default for StorybookConfig {
    fn default() -> Self {
        Self {
            title: "Space3d Storybook".into(),
            width: 1400,
            height: 860,
            screenshot_dir: "screenshots".into(),
            headless: None,
            exit_after_frames: None,
            initial_story: None,
        }
    }
}

impl StorybookConfig {
    /// Apply `SPACE3D_HEADLESS` (`1` or `WxH`) and `SPACE3D_EXIT_AFTER_FRAMES`.
    pub fn with_env(mut self) -> Self {
        let (headless, frames) = crate::env_overrides((self.width, self.height));
        self.headless = self.headless.or(headless);
        self.exit_after_frames = self.exit_after_frames.or(frames);
        self
    }
}

/// Wall-clock time per host frame spent on "Step N" ticks (the rest continues next frame).
const STEP_BUDGET: Duration = Duration::from_millis(8);

/// Build the app for one story: input, render (renderer added separately), UI, the story's
/// setup with `knobs`, then its deterministic warm-up ticks. Panics become `Err` when panics
/// unwind (see [`crate::panics_are_isolated`]; with `panic = "abort"` they abort).
pub fn build_story_app(story: &Story, knobs: &KnobValues) -> Result<App, String> {
    std::panic::catch_unwind(AssertUnwindSafe(|| {
        let mut app = App::new();
        app.add_plugin(InputPlugin);
        app.add_plugin(RenderPlugin::default());
        app.add_plugin(UiPlugin {
            pixels_per_point: Some(1.0),
            ..Default::default()
        });
        story.apply(&mut app, knobs);
        app.step_ticks(story.ticks);
        app
    }))
    .map_err(|p| format!("story {:?} panicked: {}", story.id, panic_message(p.as_ref())))
}

#[derive(Default)]
struct GroupNode {
    name: String,
    groups: Vec<GroupNode>,
    stories: Vec<usize>,
}

impl GroupNode {
    fn insert(&mut self, path: &[&str], story: usize) {
        match path.split_first() {
            None => self.stories.push(story),
            Some((first, rest)) => {
                let i = match self.groups.iter().position(|g| g.name == *first) {
                    Some(i) => i,
                    None => {
                        self.groups.push(GroupNode {
                            name: (*first).to_owned(),
                            ..Default::default()
                        });
                        self.groups.len() - 1
                    }
                };
                self.groups[i].insert(rest, story);
            }
        }
    }
}

/// Storybook viewer state (resource inserted by [`StorybookPlugin`]).
pub struct StorybookViewer {
    book: Storybook,
    selected: Option<String>,
    knobs: KnobValues,
    /// Sidebar search text (id substring or exact tag).
    pub search: String,
    /// Advance the story in real time.
    pub playing: bool,
    /// Tick count of "Step N".
    pub step_count: u64,
    view: Option<LiveView>,
    dirty: bool,
    since_build: f64,
    pending_steps: u64,
    status: String,
    screenshot_dir: String,
    writes: IoJobs<String>,
}

impl std::fmt::Debug for StorybookViewer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorybookViewer")
            .field("stories", &self.book.len())
            .field("selected", &self.selected)
            .field("status", &self.status)
            .finish()
    }
}

impl StorybookViewer {
    /// Viewer over `book`, selecting `config.initial_story` or the first story.
    pub fn new(book: Storybook, config: &StorybookConfig) -> Self {
        let mut v = Self {
            book,
            selected: None,
            knobs: KnobValues::default(),
            search: String::new(),
            playing: true,
            step_count: 10,
            view: None,
            dirty: false,
            since_build: 0.0,
            pending_steps: 0,
            status: String::new(),
            screenshot_dir: config.screenshot_dir.clone(),
            writes: IoJobs::default(),
        };
        let first = config
            .initial_story
            .clone()
            .or_else(|| v.book.iter().next().map(|s| s.id.clone()));
        if let Some(id) = first {
            v.select(&id);
        }
        v
    }

    /// The storybook.
    pub fn book(&self) -> &Storybook {
        &self.book
    }

    /// Select a story (knobs reset to defaults; the app is rebuilt next frame).
    pub fn select(&mut self, id: &str) -> bool {
        let Some(story) = self.book.get(id) else {
            return false;
        };
        self.knobs = story.default_knobs();
        self.selected = Some(id.to_owned());
        self.dirty = true;
        self.view = None;
        self.pending_steps = 0;
        true
    }

    /// Selected story.
    pub fn selected(&self) -> Option<&Story> {
        self.selected.as_deref().and_then(|id| self.book.get(id))
    }

    /// Current knob values.
    pub fn knobs(&self) -> &KnobValues {
        &self.knobs
    }

    /// Change a knob (the story app is rebuilt).
    pub fn set_knob(&mut self, name: &str, value: KnobValue) {
        let value = match self.selected().and_then(|s| s.knobs.iter().find(|k| k.name == name)) {
            Some(k) => k.clamp(value),
            None => value,
        };
        self.knobs.set(name, value);
        self.dirty = true;
    }

    /// The running story app.
    pub fn view(&self) -> Option<&LiveView> {
        self.view.as_ref()
    }

    /// Status line.
    pub fn status(&self) -> &str {
        &self.status
    }

    /// Rebuild the story app now.
    pub fn rebuild(&mut self) {
        self.dirty = false;
        self.since_build = 0.0;
        self.pending_steps = 0;
        let Some(story) = self.selected().cloned() else {
            self.view = None;
            return;
        };
        match build_story_app(&story, &self.knobs) {
            Ok(app) => {
                self.view = Some(LiveView::new(app, story.size.0, story.size.1));
                self.status = format!("{} · {} warm-up ticks", story.id, story.ticks);
            }
            Err(e) => {
                self.view = None;
                self.status = e;
            }
        }
    }

    /// Pause and run `n` more fixed ticks of the story over the next host frames (a few
    /// milliseconds of ticks per frame, without rendering the intermediate ticks), so large
    /// counts never freeze the viewer. See [`Self::pending_steps`].
    pub fn step(&mut self, n: u64) {
        self.playing = false;
        if self.view.is_some() {
            self.pending_steps = self.pending_steps.saturating_add(n);
        }
    }

    /// Ticks requested by [`Self::step`] that have not run yet.
    pub fn pending_steps(&self) -> u64 {
        self.pending_steps
    }

    /// Play or pause the story (playing drops pending steps).
    pub fn set_playing(&mut self, playing: bool) {
        self.playing = playing;
        if playing {
            self.pending_steps = 0;
        }
    }

    /// Fixed ticks the story app has simulated.
    pub fn story_tick(&self) -> Option<u64> {
        self.view
            .as_ref()?
            .app()
            .with(|a| a.resources.get::<Time>().map(|t| t.tick))
            .flatten()
    }

    /// Save the latest frame as `<screenshot_dir>/<story id>.png` (web: download); the PNG is
    /// encoded and written in the background. Returns the path.
    pub fn capture_screenshot(&mut self) -> Result<String, String> {
        let story = self.selected().ok_or("no story selected")?;
        let name = format!("{}/{}.png", self.screenshot_dir, sanitize_file_name(&story.id));
        let img = self
            .view
            .as_ref()
            .and_then(LiveView::image)
            .ok_or("no frame yet")?
            .clone();
        self.writes.push(
            "screenshot",
            write_file_with(name.clone(), move || {
                space3d_assets::edit::texture::encode_png(&img).map_err(|e| e.to_string())
            }),
        );
        self.status = format!("saving {name}...");
        Ok(name)
    }

    /// Whether a screenshot is still being written.
    pub fn io_pending(&self) -> bool {
        !self.writes.is_empty()
    }

    /// Drive the story app for one host frame: rebuild after knob changes (debounced while a
    /// slider is dragged), run pending steps, advance it, and stream its frame into the egui
    /// texture.
    pub fn frame(&mut self, ctx: &egui::Context, dt: Duration, pointer_down: bool) {
        for (_, r) in self.writes.poll() {
            self.status = r.unwrap_or_else(|e| e);
        }
        self.since_build += dt.as_secs_f64();
        if self.dirty && (!pointer_down || self.since_build > 0.35) {
            self.rebuild();
        }
        let Some(view) = &mut self.view else { return };
        if self.pending_steps > 0 {
            let ran = view.fast_forward(self.pending_steps, STEP_BUDGET);
            self.pending_steps = self.pending_steps.saturating_sub(ran);
        }
        view.update(if self.playing { dt } else { Duration::ZERO });
        view.pump(ctx);
        if let Some(e) = view.error() {
            self.status = e.to_owned();
        }
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        panel_title(ui, "Stories", &self.book.len().to_string());
        ui.add(
            egui::TextEdit::singleline(&mut self.search)
                .hint_text("search id or tag")
                .desired_width(f32::INFINITY),
        );
        let mut clicked: Option<String> = None;
        egui::ScrollArea::vertical()
            .id_salt("space3d_story_list")
            .show(ui, |ui| {
                if self.search.trim().is_empty() {
                    let mut root = GroupNode::default();
                    for (i, s) in self.book.iter().enumerate() {
                        let path: Vec<&str> = s.group.split('/').filter(|p| !p.is_empty()).collect();
                        root.insert(&path, i);
                    }
                    let stories: Vec<&Story> = self.book.iter().collect();
                    self.tree(ui, &root, &stories, &mut clicked);
                } else {
                    for s in self.book.filter(self.search.trim()) {
                        let sel = self.selected.as_deref() == Some(s.id.as_str());
                        if ui.selectable_label(sel, &s.id).clicked() {
                            clicked = Some(s.id.clone());
                        }
                    }
                }
            });
        if let Some(id) = clicked {
            self.select(&id);
        }
    }

    fn tree(&self, ui: &mut egui::Ui, node: &GroupNode, stories: &[&Story], clicked: &mut Option<String>) {
        for g in &node.groups {
            egui::CollapsingHeader::new(&g.name)
                .id_salt(("space3d_story_group", &g.name, g.stories.first()))
                .default_open(true)
                .show(ui, |ui| self.tree(ui, g, stories, clicked));
        }
        for &i in &node.stories {
            let s = stories[i];
            let sel = self.selected.as_deref() == Some(s.id.as_str());
            let r = ui.selectable_label(sel, &s.name);
            if r.clicked() {
                *clicked = Some(s.id.clone());
            }
            if !s.description.is_empty() {
                r.on_hover_text(&s.description);
            }
        }
    }

    fn knob_widget(ui: &mut egui::Ui, knob: &Knob, value: &mut KnobValue) -> bool {
        let before = value.clone();
        property(ui, &knob.name, |ui| match (&knob.kind, &mut *value) {
            (KnobKind::Bool, KnobValue::Bool(b)) => {
                ui.checkbox(b, "");
            }
            (KnobKind::Float { min, max }, KnobValue::Float(v)) => {
                fill_slider(ui, egui::Slider::new(v, *min..=*max));
            }
            (KnobKind::Int { min, max }, KnobValue::Int(v)) => {
                fill_slider(ui, egui::Slider::new(v, *min..=*max));
            }
            (KnobKind::Color, KnobValue::Color(c)) => {
                let mut rgba = egui::Rgba::from_rgba_unmultiplied(c[0], c[1], c[2], c[3]);
                if egui::color_picker::color_edit_button_rgba(ui, &mut rgba, egui::color_picker::Alpha::OnlyBlend)
                    .changed()
                {
                    *c = rgba.to_rgba_unmultiplied();
                }
            }
            (KnobKind::Text, KnobValue::Text(t)) => {
                ui.add(egui::TextEdit::singleline(t).desired_width(f32::INFINITY));
            }
            (KnobKind::Choice { options }, KnobValue::Choice(i)) => {
                egui::ComboBox::from_id_salt(("space3d_knob", &knob.name))
                    .width(ui.available_width())
                    .selected_text(options.get(*i).cloned().unwrap_or_default())
                    .show_ui(ui, |ui| {
                        for (k, o) in options.iter().enumerate() {
                            ui.selectable_value(i, k, o);
                        }
                    });
            }
            (_, v) => {
                if ui.button("reset").clicked() {
                    *v = knob.default.clone();
                } else {
                    ui.label(knob.format_value(v));
                }
            }
        });
        *value != before
    }

    fn side_panel(&mut self, ui: &mut egui::Ui) {
        let Some(story) = self.selected().cloned() else {
            ui.label(egui::RichText::new("No story selected.").weak());
            return;
        };
        panel_title(ui, &story.name, &format!("{}×{}", story.size.0, story.size.1));
        ui.label(egui::RichText::new(&story.id).small());
        if !story.description.is_empty() {
            ui.label(&story.description);
        }
        let mut info = format!("{} warm-up ticks", story.ticks);
        if !story.tags.is_empty() {
            info.push_str(" · ");
            info.push_str(&story.tags.join(", "));
        }
        ui.label(egui::RichText::new(info).small());
        ui.add_space(2.0);
        let mut reset = false;
        let knobs = story.knobs.len();
        section(
            ui,
            "space3d_story_knobs_section",
            &format!("Knobs ({knobs})"),
            true,
            |ui| {
                reset = knobs > 0 && ui.button("Reset").clicked();
            },
            |ui| {
                if knobs == 0 {
                    ui.label(egui::RichText::new("(none)").weak());
                }
                for knob in &story.knobs {
                    let mut value = self
                        .knobs
                        .get(&knob.name)
                        .cloned()
                        .unwrap_or_else(|| knob.default.clone());
                    if Self::knob_widget(ui, knob, &mut value) {
                        self.set_knob(&knob.name, value);
                    }
                }
            },
        );
        if reset {
            self.knobs = story.default_knobs();
            self.dirty = true;
        }
        section(
            ui,
            "space3d_story_sim_section",
            "Simulation",
            true,
            |_| {},
            |ui| {
                ui.horizontal(|ui| {
                    if ui.button(if self.playing { "Pause" } else { "Play" }).clicked() {
                        self.set_playing(!self.playing);
                    }
                    if ui.button("Step").clicked() {
                        self.step(1);
                    }
                    if ui.button("Restart").clicked() {
                        self.rebuild();
                    }
                });
                property(ui, "Step N", |ui| {
                    if ui.button("Run").clicked() {
                        self.step(self.step_count);
                    }
                    fill_drag(
                        ui,
                        egui::DragValue::new(&mut self.step_count)
                            .range(1..=100_000)
                            .suffix(" ticks"),
                    );
                });
                let tick = self.story_tick().map_or_else(|| "–".to_owned(), |t| t.to_string());
                let frames = self.view.as_ref().map_or(0, LiveView::frames);
                property(ui, "Tick", |ui| ui.label(format!("{tick} · {frames} frames")));
                if self.pending_steps > 0 {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(format!("stepping: {} ticks left", self.pending_steps));
                        if ui.button("Cancel").clicked() {
                            self.pending_steps = 0;
                        }
                    });
                }
            },
        );
        ui.add_space(2.0);
        if ui.button("Capture screenshot").clicked()
            && let Err(e) = self.capture_screenshot()
        {
            self.status = e;
        }
        ui.label(egui::RichText::new(&self.status).small());
        if !panics_are_isolated() {
            ui.label(egui::RichText::new("panic = abort build: a panicking story closes the viewer").small());
        }
    }

    /// Draw the viewer into the root ui (sidebar, knobs panel, live viewport).
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("space3d_story_sidebar")
            .default_size(200.0)
            .size_range(160.0..=400.0)
            .frame(panel_frame(ui.style()))
            .show(ui, |ui| {
                visible_scroll_bars(ui);
                self.sidebar(ui);
            });
        egui::Panel::right("space3d_story_knobs")
            .default_size(260.0)
            .size_range(220.0..=480.0)
            .frame(panel_frame(ui.style()))
            .show(ui, |ui| {
                visible_scroll_bars(ui);
                // Full width whatever the story shows: the panel keeps its width.
                egui::ScrollArea::vertical()
                    .id_salt("space3d_story_knobs_scroll")
                    .auto_shrink([false, true])
                    .show(ui, |ui| self.side_panel(ui));
            });
        let m = ui.style().spacing.window_margin;
        let central = egui::Frame::central_panel(ui.style()).inner_margin(m);
        egui::CentralPanel::default().frame(central).show(ui, |ui| {
            let avail = ui.available_size();
            ui.centered_and_justified(|ui| match &mut self.view {
                Some(v) => {
                    v.show(ui, avail);
                }
                None => {
                    ui.label(if self.status.is_empty() {
                        "Select a story"
                    } else {
                        &self.status
                    });
                }
            });
        });
    }
}

/// Adds the [`StorybookViewer`] resource and its UI system (needs [`UiPlugin`]).
pub struct StorybookPlugin {
    /// Stories to browse.
    pub book: Storybook,
    /// Viewer settings.
    pub config: StorybookConfig,
}

impl Plugin for StorybookPlugin {
    fn build(&self, app: &mut App) {
        if !app.has_plugin(std::any::type_name::<UiPlugin>()) {
            app.add_plugin(UiPlugin::default());
        }
        app.insert_resource(StorybookViewer::new(self.book.clone(), &self.config));
        app.add_system_named(Stage::Update, "storybook::ui", storybook_system);
    }
}

fn storybook_system(sys: &mut SystemCtx) {
    let Some(ctx) = EguiContext::active_context(sys) else {
        return;
    };
    let dt = sys.resources.get::<Time>().map_or(Duration::ZERO, |t| t.delta);
    let Some(mut viewer) = sys.resources.remove::<StorybookViewer>() else {
        return;
    };
    EguiContext::with_root_ui(sys, |ui, _| viewer.ui(ui));
    let pointer_down = ctx.input(|i| i.pointer.any_down());
    viewer.frame(&ctx, dt, pointer_down);
    sys.resources.insert(viewer);
}

#[cfg(test)]
mod tests {
    use super::*;
    use space3d_core::Stage;

    #[derive(Default)]
    struct Ticked(u64);

    fn book() -> Storybook {
        let mut b = Storybook::new();
        b.add(
            Story::new("Demo/Counter", |app, k| {
                app.insert_resource(Ticked(k.i64("start") as u64));
                app.add_system(Stage::FixedUpdate, |c| c.resources.expect_mut::<Ticked>().0 += 1);
            })
            .knob_i64("start", 5, 0..=100)
            .ticks(3)
            .size(32, 32),
        );
        b.add(Story::new("Demo/Broken", |_, _| panic!("bad story")));
        b
    }

    #[test]
    fn select_knobs_and_rebuild() {
        let mut v = StorybookViewer::new(book(), &StorybookConfig::default());
        assert_eq!(v.selected().unwrap().id, "Demo/Counter");
        v.rebuild();
        let ticks = v
            .view()
            .unwrap()
            .app()
            .with(|a| a.resources.expect::<Ticked>().0)
            .unwrap();
        assert_eq!(ticks, 8, "knob default 5 + 3 warm-up ticks");
        v.set_knob("start", KnobValue::Int(1000));
        assert_eq!(v.knobs().i64("start"), 100, "clamped to the knob range");
        v.rebuild();
        // Steps run over the following frames (non-blocking), exactly `n` ticks in total.
        v.step(2);
        v.step(40);
        assert!(!v.playing);
        assert_eq!(v.story_tick(), Some(3), "nothing ran yet");
        let ctx = egui::Context::default();
        for _ in 0..200 {
            if v.pending_steps() == 0 {
                break;
            }
            v.frame(&ctx, Duration::from_millis(16), false);
        }
        assert_eq!(v.pending_steps(), 0);
        assert_eq!(v.story_tick(), Some(45));
        v.step(5);
        v.set_playing(true);
        assert_eq!(v.pending_steps(), 0, "play drops pending steps");

        if panics_are_isolated() {
            assert!(v.select("Demo/Broken"));
            v.rebuild();
            assert!(v.view().is_none());
            assert!(v.status().contains("bad story"), "{}", v.status());
        }
        assert!(!v.select("missing"));
    }

    #[test]
    fn group_tree() {
        let mut root = GroupNode::default();
        root.insert(&["A", "B"], 0);
        root.insert(&["A"], 1);
        root.insert(&[], 2);
        assert_eq!(root.stories, vec![2]);
        assert_eq!(root.groups[0].stories, vec![1]);
        assert_eq!(root.groups[0].groups[0].stories, vec![0]);
    }
}
