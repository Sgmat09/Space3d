//! Per-system profiler built on the core's `system` / `frame` trace spans.
//!
//! `space3d_core` wraps every system run in `trace_span!("system", name = ...)` and every
//! `App::update` in `trace_span!("frame")`. [`ProfilerLayer`] (a `tracing_subscriber` layer)
//! times those spans on each thread and publishes one [`FrameTimeline`] per finished
//! outermost frame. The [`Profiler`] resource turns the timelines of its app thread into
//! per-system statistics and draws them (F4): a flame-style timeline of the last frame and a
//! table sorted by average time.

use space3d_core::SystemCtx;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::ThreadId;
use tracing::Subscriber;
use tracing::span::{Attributes, Id};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use web_time::Instant;

static ENABLED: AtomicBool = AtomicBool::new(true);

/// Whether span timing is on (global; see [`Profiler::set_enabled`]).
pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// One timed system run inside a frame.
#[derive(Clone, Debug, PartialEq)]
pub struct TimelineSpan {
    /// System name.
    pub name: Arc<str>,
    /// Start relative to the frame start, in seconds.
    pub start: f64,
    /// Duration in seconds.
    pub duration: f64,
    /// Nesting depth (0 = systems of the outermost app; nested apps' systems are deeper).
    pub depth: u32,
}

/// All system spans of one finished outermost frame.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FrameTimeline {
    /// Sequential number of the frame on its thread.
    pub index: u64,
    /// Frame duration in seconds.
    pub duration: f64,
    /// System spans in start order.
    pub spans: Vec<TimelineSpan>,
}

#[derive(Default)]
struct Recorder {
    frame_depth: u32,
    frame_start: Option<Instant>,
    system_depth: u32,
    current: Vec<TimelineSpan>,
    frames: u64,
}

thread_local! {
    static RECORDER: RefCell<Recorder> = RefCell::new(Recorder::default());
}

static PUBLISHED: parking_lot::Mutex<Option<HashMap<ThreadId, FrameTimeline>>> = parking_lot::Mutex::new(None);
static NAMES: parking_lot::Mutex<Option<HashMap<String, Arc<str>>>> = parking_lot::Mutex::new(None);

fn intern(name: &str) -> Arc<str> {
    let mut names = NAMES.lock();
    let map = names.get_or_insert_with(HashMap::new);
    if let Some(n) = map.get(name) {
        return n.clone();
    }
    let n: Arc<str> = Arc::from(name);
    map.insert(name.to_owned(), n.clone());
    n
}

/// Last published frame of a thread.
fn published(thread: ThreadId) -> Option<FrameTimeline> {
    PUBLISHED.lock().as_ref()?.get(&thread).cloned()
}

struct SpanTiming {
    name: Arc<str>,
    frame: bool,
    start: Option<Instant>,
    depth: u32,
}

#[derive(Default)]
struct NameVisitor(Option<String>);

impl tracing::field::Visit for NameVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "name" {
            self.0 = Some(value.to_owned());
        }
    }
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "name" && self.0.is_none() {
            self.0 = Some(format!("{value:?}"));
        }
    }
}

/// `tracing` layer timing the core's `system` and `frame` spans (see the module docs).
/// Installed by [`crate::install_tracing`]; add it to your own subscriber otherwise.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProfilerLayer {
    _private: (),
}

impl ProfilerLayer {
    /// New layer.
    pub fn new() -> Self {
        Self::default()
    }
}

impl<S> Layer<S> for ProfilerLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let meta = attrs.metadata();
        let frame = meta.name() == "frame";
        if !frame && meta.name() != "system" {
            return;
        }
        let name = if frame {
            intern("frame")
        } else {
            let mut v = NameVisitor::default();
            attrs.record(&mut v);
            intern(v.0.as_deref().unwrap_or("?"))
        };
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(SpanTiming {
                name,
                frame,
                start: None,
                depth: 0,
            });
        }
    }

    fn on_enter(&self, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut ext = span.extensions_mut();
        let Some(t) = ext.get_mut::<SpanTiming>() else { return };
        let now = Instant::now();
        t.start = Some(now);
        RECORDER.with(|r| {
            let mut r = r.borrow_mut();
            if t.frame {
                if r.frame_depth == 0 {
                    r.frame_start = Some(now);
                }
                r.frame_depth += 1;
            } else {
                t.depth = r.system_depth;
                r.system_depth += 1;
            }
        });
    }

    fn on_exit(&self, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let ext = span.extensions();
        let Some(t) = ext.get::<SpanTiming>() else { return };
        let Some(start) = t.start else { return };
        let now = Instant::now();
        RECORDER.with(|r| {
            let mut r = r.borrow_mut();
            if t.frame {
                r.frame_depth = r.frame_depth.saturating_sub(1);
                if r.frame_depth == 0 {
                    r.frames += 1;
                    let timeline = FrameTimeline {
                        index: r.frames,
                        duration: (now - start).as_secs_f64(),
                        spans: std::mem::take(&mut r.current),
                    };
                    r.frame_start = None;
                    PUBLISHED
                        .lock()
                        .get_or_insert_with(HashMap::new)
                        .insert(std::thread::current().id(), timeline);
                }
            } else {
                r.system_depth = r.system_depth.saturating_sub(1);
                let origin = r.frame_start.unwrap_or(start);
                let span = TimelineSpan {
                    name: t.name.clone(),
                    start: start.saturating_duration_since(origin).as_secs_f64(),
                    duration: (now - start).as_secs_f64(),
                    depth: t.depth,
                };
                r.current.push(span);
            }
        });
    }
}

/// Aggregated timings of one system.
#[derive(Clone, Debug, PartialEq)]
pub struct SystemStats {
    /// System name.
    pub name: Arc<str>,
    /// Time in the last frame (ms, all runs summed).
    pub last_ms: f64,
    /// Exponential moving average (ms per frame).
    pub avg_ms: f64,
    /// Maximum per-frame time seen (ms, slowly decaying).
    pub max_ms: f64,
    /// Runs in the last frame.
    pub calls: u32,
    /// Total runs observed.
    pub total_calls: u64,
}

/// Per-system profiler (resource inserted by [`crate::UiPlugin`]; toggle with F4).
#[derive(Debug, Default)]
pub struct Profiler {
    /// Show the profiler window.
    pub visible: bool,
    /// Pause updating the statistics (inspect a frame).
    pub frozen: bool,
    stats: Vec<SystemStats>,
    timeline: FrameTimeline,
    last_index: Option<u64>,
    frames: u64,
}

impl Profiler {
    /// Turn span timing on/off process-wide (off removes nearly all profiling overhead).
    pub fn set_enabled(enabled: bool) {
        ENABLED.store(enabled, Ordering::Relaxed);
    }

    /// Statistics sorted by average time, slowest first.
    pub fn stats(&self) -> &[SystemStats] {
        &self.stats
    }

    /// Statistics of one system.
    pub fn get(&self, name: &str) -> Option<&SystemStats> {
        self.stats.iter().find(|s| &*s.name == name)
    }

    /// Timeline of the last finished frame.
    pub fn timeline(&self) -> &FrameTimeline {
        &self.timeline
    }

    /// Frames aggregated so far.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Pull the latest finished frame of the calling thread (done every frame by the plugin).
    pub fn update(&mut self) {
        if self.frozen {
            return;
        }
        let Some(tl) = published(std::thread::current().id()) else {
            return;
        };
        if self.last_index == Some(tl.index) {
            return;
        }
        self.last_index = Some(tl.index);
        self.ingest(tl);
    }

    fn ingest(&mut self, tl: FrameTimeline) {
        self.frames += 1;
        let mut per_frame: HashMap<Arc<str>, (f64, u32)> = HashMap::new();
        for s in &tl.spans {
            let e = per_frame.entry(s.name.clone()).or_default();
            e.0 += s.duration * 1000.0;
            e.1 += 1;
        }
        for s in &mut self.stats {
            let (ms, calls) = per_frame.remove(&s.name).unwrap_or((0.0, 0));
            s.last_ms = ms;
            s.avg_ms += (ms - s.avg_ms) * 0.1;
            s.max_ms = (s.max_ms * 0.995).max(ms);
            s.calls = calls;
            s.total_calls += calls as u64;
        }
        for (name, (ms, calls)) in per_frame {
            self.stats.push(SystemStats {
                name,
                last_ms: ms,
                avg_ms: ms,
                max_ms: ms,
                calls,
                total_calls: calls as u64,
            });
        }
        self.stats
            .sort_by(|a, b| b.avg_ms.total_cmp(&a.avg_ms).then_with(|| a.name.cmp(&b.name)));
        self.timeline = tl;
    }

    /// Draw the profiler contents into `ui`.
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.frozen, "Freeze");
            let mut on = is_enabled();
            if ui.checkbox(&mut on, "Timing").changed() {
                Self::set_enabled(on);
            }
            ui.label(format!(
                "frame {:.2} ms · {} spans",
                self.timeline.duration * 1000.0,
                self.timeline.spans.len()
            ));
        });
        if self.stats.is_empty() {
            ui.label(match crate::tracing_install_state() {
                crate::TracingInstall::None => {
                    "No timings: install the tracing layers (UiPlugin::install_tracing or ProfilerLayer)."
                }
                _ => "Waiting for frames...",
            });
            return;
        }
        ui.separator();
        self.flame_ui(ui);
        ui.separator();
        self.table_ui(ui);
    }

    fn flame_ui(&self, ui: &mut egui::Ui) {
        let tl = &self.timeline;
        let depth = tl.spans.iter().map(|s| s.depth).max().unwrap_or(0) + 1;
        let row = 16.0;
        let width = ui.available_width().max(100.0);
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(width, row * depth as f32 + 4.0), egui::Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);
        let total = tl.duration.max(1e-9);
        let hover = response.hover_pos();
        let mut tooltip = None;
        for s in &tl.spans {
            let x0 = rect.left() + (s.start / total) as f32 * rect.width();
            let w = ((s.duration / total) as f32 * rect.width()).max(1.0);
            let y0 = rect.top() + 2.0 + s.depth as f32 * row;
            let r = egui::Rect::from_min_size(egui::pos2(x0, y0), egui::vec2(w, row - 1.0));
            painter.rect_filled(r, 1.0, name_color(&s.name));
            if w > 40.0 {
                painter.text(
                    r.left_center() + egui::vec2(2.0, 0.0),
                    egui::Align2::LEFT_CENTER,
                    short_name(&s.name),
                    egui::FontId::monospace(10.0),
                    egui::Color32::BLACK,
                );
            }
            if hover.is_some_and(|p| r.contains(p)) {
                tooltip = Some(format!("{}\n{:.3} ms", s.name, s.duration * 1000.0));
            }
        }
        if let Some(t) = tooltip {
            response.on_hover_text(t);
        }
    }

    fn table_ui(&self, ui: &mut egui::Ui) {
        let max = self.stats.first().map_or(1.0, |s| s.avg_ms.max(1e-6));
        egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
            egui::Grid::new("space3d_profiler_table")
                .striped(true)
                .num_columns(5)
                .show(ui, |ui| {
                    ui.strong("system");
                    ui.strong("avg ms");
                    ui.strong("last ms");
                    ui.strong("max ms");
                    ui.strong("calls");
                    ui.end_row();
                    for s in &self.stats {
                        ui.horizontal(|ui| {
                            let (r, _) = ui.allocate_exact_size(egui::vec2(60.0, 10.0), egui::Sense::hover());
                            let frac = (s.avg_ms / max) as f32;
                            ui.painter().rect_filled(
                                egui::Rect::from_min_size(r.min, egui::vec2(r.width() * frac, r.height())),
                                1.0,
                                name_color(&s.name),
                            );
                            ui.label(short_name(&s.name)).on_hover_text(&*s.name);
                        });
                        ui.monospace(format!("{:.3}", s.avg_ms));
                        ui.monospace(format!("{:.3}", s.last_ms));
                        ui.monospace(format!("{:.3}", s.max_ms));
                        ui.monospace(format!("{}", s.calls));
                        ui.end_row();
                    }
                });
        });
    }
}

/// Stable pastel color for a name.
fn name_color(name: &str) -> egui::Color32 {
    let mut h: u32 = 2_166_136_261;
    for b in name.bytes() {
        h = (h ^ b as u32).wrapping_mul(16_777_619);
    }
    let hue = (h % 360) as f32 / 360.0;
    egui::ecolor::Hsva::new(hue, 0.45, 0.9, 1.0).into()
}

/// Strip module paths from closure-style type names (`a::b::{{closure}}` → `b::{{closure}}`).
fn short_name(name: &str) -> &str {
    let trimmed = name.trim_end_matches("::{{closure}}");
    match trimmed.rmatch_indices("::").nth(1) {
        Some((i, _)) => &name[i + 2..],
        None => name,
    }
}

/// `Stage::Update`: pull timings and draw the window.
pub(crate) fn profiler_system(sys: &mut SystemCtx) {
    let ctx = crate::EguiContext::active_context(sys);
    let Some(p) = sys.resources.get_mut::<Profiler>() else {
        return;
    };
    p.update();
    let Some(ctx) = ctx else { return };
    if !p.visible {
        return;
    }
    let mut open = true;
    egui::Window::new("Profiler")
        .id(egui::Id::new("space3d_profiler"))
        .open(&mut open)
        .default_pos([360.0, 40.0])
        .default_width(520.0)
        .show(&ctx, |ui| p.ui(ui));
    if !open {
        p.visible = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_names_and_aggregation() {
        assert_eq!(short_name("render::render_frame"), "render::render_frame");
        assert_eq!(short_name("a::b::c::d"), "c::d");
        assert_eq!(short_name("app::sys::{{closure}}"), "app::sys::{{closure}}");
        let mut p = Profiler::default();
        let span = |n: &str, d: f64| TimelineSpan {
            name: intern(n),
            start: 0.0,
            duration: d,
            depth: 0,
        };
        p.ingest(FrameTimeline {
            index: 1,
            duration: 0.01,
            spans: vec![span("a", 0.002), span("b", 0.004), span("a", 0.001)],
        });
        assert_eq!(p.stats()[0].name.as_ref(), "b");
        let a = p.get("a").unwrap();
        assert_eq!(a.calls, 2);
        assert!((a.last_ms - 3.0).abs() < 1e-9);
        p.ingest(FrameTimeline {
            index: 2,
            duration: 0.01,
            spans: vec![span("a", 0.0)],
        });
        assert_eq!(p.get("b").unwrap().calls, 0);
        assert_eq!(p.get("a").unwrap().total_calls, 3);
    }
}
