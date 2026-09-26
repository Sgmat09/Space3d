//! Per-system profiler built on the core's `system` / `frame` trace spans.
//!
//! `space3d_core` wraps every system run in `trace_span!("system", name = ...)` and every
//! `App::update` in `trace_span!("frame")`. [`ProfilerLayer`] (a `tracing_subscriber` layer)
//! times those spans on each thread and publishes one [`FrameTimeline`] per finished
//! outermost frame. The [`Profiler`] resource turns the timelines of its app thread into
//! per-system statistics and draws them (F4): a flame-style timeline of the last frame and a
//! table sorted by average time.

use space3d_core::SystemCtx;
use std::borrow::Cow;
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
        self.record(tl);
    }

    /// Aggregate one finished frame into the statistics and make it the shown timeline (what
    /// [`Self::update`] does with the frames of [`ProfilerLayer`]). For other sources of
    /// timings: recorded sessions, replays, tests. Works while [`Self::frozen`].
    pub fn record(&mut self, tl: FrameTimeline) {
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

    /// Draw the profiler contents into `ui`: a toolbar, then collapsible "Timeline" (the last
    /// frame's systems, flame style) and "Systems" (a striped table sorted by average time,
    /// numbers right-aligned, scrolling by whole rows within the space left in `ui`) sections,
    /// both the full width of `ui` (not indented).
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let bottom = ui.available_rect_before_wrap().bottom();
        self.ui_until(ui, bottom);
    }

    /// [`Self::ui`] with the table ending above screen y `max_bottom`.
    fn ui_until(&mut self, ui: &mut egui::Ui, max_bottom: f32) {
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.frozen, "Freeze");
            let mut on = is_enabled();
            if ui.checkbox(&mut on, "Timing").changed() {
                Self::set_enabled(on);
            }
        });
        if self.stats.is_empty() {
            ui.label(
                egui::RichText::new(match crate::tracing_install_state() {
                    crate::TracingInstall::None => {
                        "No timings: install the tracing layers (UiPlugin::install_tracing or ProfilerLayer)."
                    }
                    _ => "Waiting for frames...",
                })
                .weak(),
            );
            return;
        }
        egui::CollapsingHeader::new("Timeline")
            .id_salt("space3d_profiler_timeline")
            .default_open(true)
            .show_unindented(ui, |ui| self.flame_ui(ui));
        egui::CollapsingHeader::new(format!("Systems ({})", self.stats.len()))
            .id_salt("space3d_profiler_systems")
            .default_open(true)
            .show_unindented(ui, |ui| self.table_ui(ui, max_bottom));
    }

    /// One-line summary of the shown frame (time, span count, frozen), as in the F4 window's
    /// header; [`Self::ui`] leaves it out for hosts that show it elsewhere.
    pub fn summary(&self) -> String {
        format!(
            "{:.2} ms · {} spans{}",
            self.timeline.duration * 1000.0,
            self.timeline.spans.len(),
            if self.frozen { " · frozen" } else { "" }
        )
    }

    fn flame_ui(&self, ui: &mut egui::Ui) {
        let tl = &self.timeline;
        let depth = tl.spans.iter().map(|s| s.depth).max().unwrap_or(0) + 1;
        let row = 14.0;
        let width = ui.available_width().max(100.0);
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(width, row * depth as f32 + 2.0), egui::Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);
        let total = tl.duration.max(1e-9);
        let hover = response.hover_pos();
        let mut tooltip = None;
        let font = egui::FontId::proportional(10.5);
        for s in &tl.spans {
            let x0 = rect.left() + (s.start / total) as f32 * rect.width();
            let w = ((s.duration / total) as f32 * rect.width()).max(1.0);
            let y0 = rect.top() + 1.0 + s.depth as f32 * row;
            let r = egui::Rect::from_min_size(egui::pos2(x0, y0), egui::vec2(w, row - 1.0));
            painter.rect_filled(r, 1.0, name_color(&s.name));
            if w > 40.0 {
                // Truncated with "…" to the span (the full name is in the tooltip).
                let mut job = egui::text::LayoutJob::single_section(
                    short_name(&s.name).into_owned(),
                    egui::TextFormat::simple(font.clone(), egui::Color32::BLACK),
                );
                job.wrap = egui::text::TextWrapping::truncate_at_width(w - 6.0);
                let galley = painter.layout_job(job);
                let pos = egui::pos2(r.left() + 3.0, r.center().y - galley.size().y / 2.0);
                painter
                    .with_clip_rect(r.intersect(rect))
                    .galley(pos, galley, egui::Color32::BLACK);
            }
            if hover.is_some_and(|p| r.contains(p)) {
                tooltip = Some(format!("{}\n{:.3} ms", s.name, s.duration * 1000.0));
            }
        }
        if let Some(t) = tooltip {
            response.on_hover_text(t);
        }
    }

    /// The systems table: a header row, then one row per system (scrolling by whole rows so no
    /// row is ever cut) ending above screen y `max_bottom`. Columns fill the width: a bar of the
    /// average time, the name (truncated, full name on hover) and right-aligned numbers.
    fn table_ui(&self, ui: &mut egui::Ui, max_bottom: f32) {
        let max = self.stats.first().map_or(1.0, |s| s.avg_ms.max(1e-6));
        let mono = egui::TextStyle::Monospace.resolve(ui.style());
        let body = egui::TextStyle::Body.resolve(ui.style());
        let (pitch, digit) = ui.fonts_mut(|f| {
            (
                (f.row_height(&mono).max(f.row_height(&body)) + 3.0).round(),
                f.glyph_width(&mono, '0'),
            )
        });
        let columns = TableColumns::new(digit);
        let visuals = ui.visuals().clone();
        let text = visuals.text_color();

        let (header, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), pitch), egui::Sense::hover());
        let painter = ui.painter();
        let x = columns.xs(header);
        let y = header.center().y;
        painter.text(
            egui::pos2(x.name, y),
            egui::Align2::LEFT_CENTER,
            "system",
            body.clone(),
            text,
        );
        for (right, h) in [(x.avg, "avg"), (x.last, "last"), (x.max, "max"), (x.calls, "calls")] {
            painter.text(egui::pos2(right, y), egui::Align2::RIGHT_CENTER, h, mono.clone(), text);
        }
        painter.hline(
            header.x_range(),
            header.bottom() - 0.5,
            egui::Stroke::new(1.0, visuals.widgets.noninteractive.bg_stroke.color),
        );

        // Stripes a little stronger than egui's barely visible `faint_bg_color`.
        let stripe = visuals.faint_bg_color.gamma_multiply(3.0);
        ui.scope(|ui| {
            crate::tool::derived_style(ui, "profiler_rows", |s| s.spacing.item_spacing.y = 0.0);
            let rows = self.stats.len();
            let room = (max_bottom - ui.cursor().top()).max(pitch);
            let shown = ((room / pitch).floor() as usize).clamp(1, rows.max(1));
            // Exactly `shown` rows tall (egui makes a scrolling area at least 64 pt tall, which
            // would cut the last row).
            let height = shown as f32 * pitch;
            egui::ScrollArea::vertical()
                .id_salt("space3d_profiler_scroll")
                .max_height(height)
                .min_scrolled_height(height)
                .auto_shrink([false, true])
                .show_rows(ui, pitch, rows, |ui, range| {
                    for i in range {
                        let s = &self.stats[i];
                        let (rect, response) =
                            ui.allocate_exact_size(egui::vec2(ui.available_width(), pitch), egui::Sense::hover());
                        let painter = ui.painter();
                        if i % 2 == 1 {
                            painter.rect_filled(rect, 0.0, stripe);
                        }
                        let x = columns.xs(rect);
                        let y = rect.center().y;
                        let bar = egui::Rect::from_min_size(
                            egui::pos2(rect.left() + 2.0, y - 3.0),
                            egui::vec2(columns.bar, 6.0),
                        );
                        let frac = (s.avg_ms / max).clamp(0.0, 1.0) as f32;
                        painter.rect_filled(bar, 1.0, egui::Color32::from_white_alpha(12));
                        painter.rect_filled(
                            egui::Rect::from_min_size(bar.min, egui::vec2(bar.width() * frac, bar.height())),
                            1.0,
                            name_color(&s.name),
                        );
                        let mut job = egui::text::LayoutJob::single_section(
                            short_name(&s.name).into_owned(),
                            egui::TextFormat::simple(body.clone(), text),
                        );
                        job.wrap = egui::text::TextWrapping::truncate_at_width((x.name_end - x.name).max(8.0));
                        let galley = ui.fonts_mut(|f| f.layout_job(job));
                        painter.galley(egui::pos2(x.name, y - galley.size().y / 2.0), galley, text);
                        for (right, value) in [
                            (x.avg, format!("{:.3}", s.avg_ms)),
                            (x.last, format!("{:.3}", s.last_ms)),
                            (x.max, format!("{:.3}", s.max_ms)),
                            (x.calls, s.calls.to_string()),
                        ] {
                            painter.text(
                                egui::pos2(right, y),
                                egui::Align2::RIGHT_CENTER,
                                value,
                                mono.clone(),
                                text,
                            );
                        }
                        response.on_hover_ui(|ui| {
                            ui.label(&*s.name);
                        });
                    }
                });
        });
    }
}

/// Column layout of the profiler table (widths from the monospace digit width).
struct TableColumns {
    bar: f32,
    number: f32,
    calls: f32,
    gap: f32,
}

/// Screen x of the table's columns in a row: where the name starts and must end, and the right
/// edges of the right-aligned number columns.
struct ColumnXs {
    name: f32,
    name_end: f32,
    avg: f32,
    last: f32,
    max: f32,
    calls: f32,
}

impl TableColumns {
    fn new(digit: f32) -> Self {
        Self {
            bar: 28.0,
            number: (digit * 8.0).ceil(),
            calls: (digit * 5.0).ceil(),
            gap: 8.0,
        }
    }

    fn xs(&self, row: egui::Rect) -> ColumnXs {
        let calls = row.right() - 2.0;
        let max = calls - self.calls - self.gap;
        let last = max - self.number - self.gap;
        let avg = last - self.number - self.gap;
        ColumnXs {
            name: row.left() + 2.0 + self.bar + 6.0,
            name_end: avg - self.number - self.gap,
            avg,
            last,
            max,
            calls,
        }
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

/// Shorten a system name: the path keeps its last two segments, generic arguments their last
/// segment and closure suffixes stay (`a::b::c::{{closure}}` → `b::c::{{closure}}`,
/// `space3d_core::events::Events<space3d_core::app::AppExit>` → `events::Events<AppExit>`).
fn short_name(name: &str) -> Cow<'_, str> {
    const CLOSURE: &str = "::{{closure}}";
    let core = name.trim_end_matches(CLOSURE);
    let suffix = &name[core.len()..];
    let Some(open) = core.find('<') else {
        return Cow::Borrowed(&name[last_segments(core, 2)..]);
    };
    let head = core[..open].trim_end_matches("::");
    let mut out = String::with_capacity(core.len());
    out.push_str(&head[last_segments(head, 2)..]);
    // Every path inside the generic arguments keeps its last segment.
    let args = &core[open..];
    let mut path_start = None;
    for (i, c) in args.char_indices() {
        let in_path = c.is_alphanumeric() || c == '_' || c == ':';
        match (in_path, path_start) {
            (true, None) => path_start = Some(i),
            (false, Some(start)) => {
                let path: &str = &args[start..i];
                out.push_str(&path[last_segments(path, 1)..]);
                path_start = None;
            }
            _ => {}
        }
        if !in_path {
            out.push(c);
        }
    }
    if let Some(start) = path_start {
        let path = &args[start..];
        out.push_str(&path[last_segments(path, 1)..]);
    }
    out.push_str(suffix);
    Cow::Owned(out)
}

/// Byte offset in `path` where its last `n` `::`-separated segments start.
fn last_segments(path: &str, n: usize) -> usize {
    path.rmatch_indices("::").nth(n - 1).map_or(0, |(i, _)| i + 2)
}

/// `Stage::Update`: pull timings and draw the window.
pub(crate) fn profiler_system(sys: &mut SystemCtx) {
    let ctx = crate::EguiContext::active_context(sys);
    let console_open = sys.resources.get::<crate::Console>().is_some_and(|c| c.visible);
    let overlay_open = sys.resources.get::<crate::DebugOverlay>().is_some_and(|d| d.visible);
    let Some(p) = sys.resources.get_mut::<Profiler>() else {
        return;
    };
    p.update();
    let Some(ctx) = ctx else { return };
    if !p.visible {
        return;
    }
    let (size, max_width) = window_size(&ctx, console_open, overlay_open);
    let summary = p.summary();
    let mut open = true;
    crate::tool::window(
        &ctx,
        crate::tool::ToolWindow {
            id: PROFILER_ID,
            title: "Profiler",
            anchor: egui::Align2::RIGHT_TOP,
            size,
            fit_height: true,
            max_width,
        },
        &mut open,
        |ui| {
            ui.label(summary);
        },
        |ui, max_bottom| p.ui_until(ui, max_bottom),
    );
    if !open {
        p.visible = false;
    }
}

const PROFILER_ID: &str = "space3d_profiler";

/// Narrowest the profiler gets to stay clear of the debug overlay.
const MIN_WIDTH: f32 = 300.0;

/// Largest size of the profiler window (docked top-right): 440 pt wide and 45 % of the screen
/// height (at least 300 pt), but never down into the console when the console is open below it
/// (at its default place, or wherever the user moved it). Also the width it must stay within
/// this frame: with the debug overlay (top-left) open beside it, the profiler ends left of it
/// (unless that would leave it under [`MIN_WIDTH`]), `f32::INFINITY` otherwise.
fn window_size(ctx: &egui::Context, console_open: bool, overlay_open: bool) -> (egui::Vec2, f32) {
    let edge = crate::tool::EDGE;
    let screen = ctx.content_rect();
    let width = 440.0f32.min(screen.width() - 2.0 * edge);
    let (profiler, console, overlay) = ctx.memory(|m| {
        (
            m.area_rect(egui::Id::new(PROFILER_ID)),
            m.area_rect(egui::Id::new(crate::console::CONSOLE_ID)),
            m.area_rect(egui::Id::new(crate::debug::OVERLAY_ID)),
        )
    });
    let profiler = profiler.unwrap_or_else(|| {
        egui::Rect::from_min_size(
            egui::pos2(screen.right() - edge - width, screen.top() + edge),
            egui::vec2(width, 0.0),
        )
    });
    // The overlay starts at the top of the screen, so a profiler whose top is beside it
    // (above its bottom) must end left of it.
    let mut max_width = f32::INFINITY;
    if overlay_open
        && let Some(overlay) = overlay
        && profiler.top() < overlay.bottom() + edge
    {
        let room = profiler.right() - overlay.right() - edge;
        if room >= MIN_WIDTH {
            max_width = room;
        }
    }
    let mut height = (screen.height() * 0.45).max(300.0).min(screen.height() - 2.0 * edge);
    if console_open {
        let console = console.unwrap_or_else(|| crate::console::window_rect(screen));
        if console.x_range().intersects(profiler.x_range()) && console.top() > profiler.top() {
            height = height.min(console.top() - edge - profiler.top());
        }
    }
    (egui::vec2(width.min(max_width), height.max(90.0)), max_width)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_names_and_aggregation() {
        assert_eq!(short_name("render::render_frame"), "render::render_frame");
        assert_eq!(short_name("a::b::c::d"), "c::d");
        assert_eq!(short_name("app::sys::{{closure}}"), "app::sys::{{closure}}");
        assert_eq!(short_name("x::app::sys::{{closure}}"), "app::sys::{{closure}}");
        assert_eq!(short_name("tick"), "tick");
        assert_eq!(
            short_name("space3d_core::events::Events<space3d_core::app::AppExit>"),
            "events::Events<AppExit>"
        );
        assert_eq!(
            short_name("a::b::c::run::<x::y::Foo, Vec<z::Bar>>::{{closure}}"),
            "c::run<Foo, Vec<Bar>>::{{closure}}"
        );
        let mut p = Profiler::default();
        let span = |n: &str, d: f64| TimelineSpan {
            name: intern(n),
            start: 0.0,
            duration: d,
            depth: 0,
        };
        p.record(FrameTimeline {
            index: 1,
            duration: 0.01,
            spans: vec![span("a", 0.002), span("b", 0.004), span("a", 0.001)],
        });
        assert_eq!(p.stats()[0].name.as_ref(), "b");
        let a = p.get("a").unwrap();
        assert_eq!(a.calls, 2);
        assert!((a.last_ms - 3.0).abs() < 1e-9);
        p.record(FrameTimeline {
            index: 2,
            duration: 0.01,
            spans: vec![span("a", 0.0)],
        });
        assert_eq!(p.get("b").unwrap().calls, 0);
        assert_eq!(p.get("a").unwrap().total_calls, 3);
    }
}
