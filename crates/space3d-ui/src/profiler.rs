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
    fit: TableFit,
}

/// The profiler's layout as last drawn in its window, for [`Profiler::min_height`].
#[derive(Clone, Copy, Debug, Default)]
struct TableFit {
    /// Height from the top of the window's contents to the first table row in the compact layout
    /// (no section headers); 0 until measured.
    rows_offset: f32,
    /// Height of a table row.
    pitch: f32,
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
    /// both the full width of `ui` (not indented). Where the sections and three table rows do
    /// not fit, the section headers are left out (the flame view and the table stay).
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let bottom = ui.available_rect_before_wrap().bottom();
        let _ = self.ui_until(ui, bottom);
    }

    /// Height of the profiler window with `rows` table rows in its compact layout (see
    /// [`Self::ui_until`]), as last drawn (estimated before the first time).
    pub(crate) fn min_height(&self, ctx: &egui::Context, rows: usize) -> f32 {
        let (offset, pitch) = if self.fit.pitch > 0.0 {
            (self.fit.rows_offset, self.fit.pitch)
        } else {
            // Header row, separator, toolbar, a one-level flame and the table's header.
            (85.0, 17.0)
        };
        crate::tool::chrome(ctx).y + offset + rows as f32 * pitch
    }

    /// [`Self::ui`] with the table ending above screen y `max_bottom`. When the sections and
    /// [`crate::dock::MIN_PROFILER_ROWS`] table rows do not fit above it, the section headers
    /// are left out (the compact layout: the flame and the table stay, always open). Returns the
    /// layout for [`Self::min_height`] when `ui` is the profiler window's (measured from the top
    /// of `ui`), if it could be measured.
    #[must_use]
    fn ui_until(&mut self, ui: &mut egui::Ui, max_bottom: f32) -> Option<TableFit> {
        let top = ui.max_rect().top();
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
            return None;
        }
        let pitch = table_pitch(ui);
        let spacing = ui.spacing().item_spacing.y;
        let section_header = ui.spacing().interact_size.y + spacing;
        let table = pitch + spacing + crate::dock::MIN_PROFILER_ROWS as f32 * pitch;
        let full = 2.0 * section_header + self.flame_height() + spacing + table;
        let rows_offset = if max_bottom - ui.cursor().top() < full {
            self.flame_ui(ui);
            Some(self.table_ui(ui, max_bottom) - top)
        } else {
            let timeline = egui::CollapsingHeader::new("Timeline")
                .id_salt("space3d_profiler_timeline")
                .default_open(true)
                .show_unindented(ui, |ui| self.flame_ui(ui));
            let systems = egui::CollapsingHeader::new(format!("Systems ({})", self.stats.len()))
                .id_salt("space3d_profiler_systems")
                .default_open(true)
                .show_unindented(ui, |ui| self.table_ui(ui, max_bottom));
            // (Measured only with both sections open: the compact layout shows both.)
            timeline.body_returned.and(systems.body_returned).map(|rows_top| {
                let headers = timeline.header_response.rect.height() + systems.header_response.rect.height();
                rows_top - top - headers - 2.0 * spacing
            })
        };
        rows_offset.map(|rows_offset| TableFit { rows_offset, pitch })
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

    /// Height of the flame view: a 14 pt row per nesting level.
    fn flame_height(&self) -> f32 {
        let depth = self.timeline.spans.iter().map(|s| s.depth).max().unwrap_or(0) + 1;
        FLAME_ROW * depth as f32 + 2.0
    }

    fn flame_ui(&self, ui: &mut egui::Ui) {
        let tl = &self.timeline;
        let row = FLAME_ROW;
        let width = ui.available_width().max(100.0);
        let (rect, response) = ui.allocate_exact_size(egui::vec2(width, self.flame_height()), egui::Sense::hover());
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
                // Truncated with "…" to the span (the full name is in the tooltip). A name that
                // fits keeps the galley of the whole name: the same from frame to frame, where
                // one truncated to the span's (changing) width is laid out anew.
                let job = |wrap| {
                    let mut job = egui::text::LayoutJob::single_section(
                        short_name(&s.name).into_owned(),
                        egui::TextFormat::simple(font.clone(), egui::Color32::BLACK),
                    );
                    job.wrap = wrap;
                    painter.layout_job(job)
                };
                let whole = job(egui::text::TextWrapping::no_max_width());
                // (egui wraps at whole points.)
                let galley = if whole.size().x <= (w - 6.0).round() {
                    whole
                } else {
                    job(egui::text::TextWrapping::truncate_at_width(w - 6.0))
                };
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
    /// average time, the name (truncated, full name on hover) and right-aligned numbers, their
    /// headers right above them whether or not a scroll bar takes a column of its own. In a
    /// narrow table the name keeps the room of the `last` and `max` columns. Returns the screen y
    /// of the first row.
    fn table_ui(&self, ui: &mut egui::Ui, max_bottom: f32) -> f32 {
        let max = self.stats.first().map_or(1.0, |s| s.avg_ms.max(1e-6));
        let mono = egui::TextStyle::Monospace.resolve(ui.style());
        let body = egui::TextStyle::Body.resolve(ui.style());
        let pitch = table_pitch(ui);
        let digit = ui.fonts_mut(|f| f.glyph_width(&mono, '0'));
        let columns = TableColumns::new(digit);
        let visuals = ui.visuals().clone();
        let text = visuals.text_color();

        // The header's texts are drawn once the rows' width is known (a scroll bar narrows them).
        let (header, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), pitch), egui::Sense::hover());
        let mut rows_x = None;

        // Stripes a little stronger than egui's barely visible `faint_bg_color`.
        let stripe = visuals.faint_bg_color.gamma_multiply(3.0);
        let mut rows_top = 0.0;
        ui.scope(|ui| {
            crate::tool::derived_style(ui, "profiler_rows", |s| s.spacing.item_spacing.y = 0.0);
            let rows = self.stats.len();
            rows_top = ui.cursor().top();
            let room = (max_bottom - rows_top).max(pitch);
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
                        rows_x.get_or_insert(rect.x_range());
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
                        let numbers = [(Some(x.avg), s.avg_ms), (x.last, s.last_ms), (x.max, s.max_ms)];
                        for (right, ms) in numbers {
                            if let Some(right) = right {
                                right_aligned(painter, egui::pos2(right, y), format!("{ms:.3}"), &mono, text);
                            }
                        }
                        right_aligned(painter, egui::pos2(x.calls, y), s.calls.to_string(), &mono, text);
                        response.on_hover_ui(|ui| {
                            ui.label(&*s.name);
                        });
                    }
                });
        });

        let header = egui::Rect::from_x_y_ranges(rows_x.unwrap_or(header.x_range()), header.y_range());
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
        let titles = [
            (Some(x.avg), "avg"),
            (x.last, "last"),
            (x.max, "max"),
            (Some(x.calls), "calls"),
        ];
        for (right, h) in titles {
            if let Some(right) = right {
                painter.text(egui::pos2(right, y), egui::Align2::RIGHT_CENTER, h, mono.clone(), text);
            }
        }
        painter.hline(
            header.x_range(),
            header.bottom() - 0.5,
            egui::Stroke::new(1.0, visuals.widgets.noninteractive.bg_stroke.color),
        );
        rows_top
    }
}

/// Height of a flame view row.
const FLAME_ROW: f32 = 14.0;

/// Height of a table row in `ui`'s style: a line of the monospace or body font, whichever is
/// taller, and 3 pt.
fn table_pitch(ui: &egui::Ui) -> f32 {
    let mono = egui::TextStyle::Monospace.resolve(ui.style());
    let body = egui::TextStyle::Body.resolve(ui.style());
    ui.fonts_mut(|f| (f.row_height(&mono).max(f.row_height(&body)) + 3.0).round())
}

/// Draw `text` with its right-center at `pos` (like `Painter::text`, without copying `text`).
fn right_aligned(painter: &egui::Painter, pos: egui::Pos2, text: String, font: &egui::FontId, color: egui::Color32) {
    let galley = painter.layout_no_wrap(text, font.clone(), color);
    let rect = egui::Align2::RIGHT_CENTER.anchor_size(pos, galley.size());
    painter.galley(rect.min, galley, color);
}

/// Column layout of the profiler table (widths from the monospace digit width).
struct TableColumns {
    bar: f32,
    number: f32,
    calls: f32,
    gap: f32,
    /// Narrowest name column before `last` and `max` give up their room.
    min_name: f32,
}

/// Screen x of the table's columns in a row: where the name starts and must end, and the right
/// edges of the right-aligned number columns (`last` and `max` are left out of a narrow table).
struct ColumnXs {
    name: f32,
    name_end: f32,
    avg: f32,
    last: Option<f32>,
    max: Option<f32>,
    calls: f32,
}

impl TableColumns {
    fn new(digit: f32) -> Self {
        Self {
            bar: 28.0,
            number: (digit * 8.0).ceil(),
            calls: (digit * 5.0).ceil(),
            gap: 8.0,
            min_name: (digit * 12.0).ceil(),
        }
    }

    fn xs(&self, row: egui::Rect) -> ColumnXs {
        let name = row.left() + 2.0 + self.bar + 6.0;
        let calls = row.right() - 2.0;
        let max = calls - self.calls - self.gap;
        let last = max - self.number - self.gap;
        let avg = last - self.number - self.gap;
        let name_end = avg - self.number - self.gap;
        if name_end - name >= self.min_name {
            ColumnXs {
                name,
                name_end,
                avg,
                last: Some(last),
                max: Some(max),
                calls,
            }
        } else {
            let avg = max;
            ColumnXs {
                name,
                name_end: avg - self.number - self.gap,
                avg,
                last: None,
                max: None,
                calls,
            }
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
    let dock = ctx.as_ref().map(|ctx| {
        let dock = crate::tool::dock_rect(sys.resources, ctx);
        (dock, crate::dock::plan(sys.resources, ctx, dock))
    });
    let Some(p) = sys.resources.get_mut::<Profiler>() else {
        return;
    };
    p.update();
    let (Some(ctx), Some((dock, plan))) = (ctx, dock) else {
        return;
    };
    if !p.visible {
        return;
    }
    let place = placement(&ctx, dock, &plan, console_open, overlay_open);
    let summary = p.summary();
    let mut open = true;
    let fit = crate::tool::window(
        &ctx,
        crate::tool::ToolWindow {
            id: PROFILER_ID,
            title: "Profiler",
            anchor: egui::Align2::RIGHT_TOP,
            dock_pos: place.dock_pos,
            bounds: place.bounds,
            size: place.size,
            fit_height: true,
            max_width: place.max_width,
        },
        &mut open,
        |ui| {
            ui.label(summary);
        },
        |ui, max_bottom| p.ui_until(ui, max_bottom),
    );
    if let Some(Some(fit)) = fit {
        p.fit = fit;
    }
    if !open {
        p.visible = false;
    }
}

/// Egui id of the profiler window.
pub(crate) const PROFILER_ID: &str = "space3d_profiler";

/// Default width of the docked profiler.
pub(crate) const WIDTH: f32 = 440.0;

/// Narrowest the profiler gets to stay clear of the debug overlay or to leave the console room
/// beside it (its table then shows the average and calls columns only).
pub(crate) const MIN_WIDTH: f32 = 240.0;

/// Height below the debug overlay the profiler needs to dock there instead (header, sections and
/// a few rows).
const MIN_STACKED_HEIGHT: f32 = 160.0;

/// Where and how big the profiler window is this frame.
struct Placement {
    /// Where its top-right corner docks.
    dock_pos: egui::Pos2,
    /// The dock area, [`crate::tool::EDGE`] in.
    bounds: egui::Rect,
    /// Largest size.
    size: egui::Vec2,
    /// Width it must stay within this frame (the debug overlay is beside it), or infinity.
    max_width: f32,
}

/// The profiler docks top-right in `dock` (the dock area), 440 pt wide and up to 45 % of the
/// area's height (at least 300 pt), but never down into the console when the console is open
/// below it (docked, or wherever the user moved it). With the debug overlay (top-left) open
/// beside it, the profiler ends left of it; when that would leave it under [`MIN_WIDTH`], it
/// docks below the overlay instead if [`MIN_STACKED_HEIGHT`] fits there (tall, narrow screens),
/// and only overlaps the overlay when neither fits. In a short area where the console docks
/// beside it (`plan`, [`crate::dock`]) it takes the area's height at the width the plan leaves.
fn placement(
    ctx: &egui::Context,
    dock: egui::Rect,
    plan: &crate::dock::DockPlan,
    console_open: bool,
    overlay_open: bool,
) -> Placement {
    let edge = crate::tool::EDGE;
    let bounds = dock.shrink(edge);
    let width = plan.profiler_beside.map_or(WIDTH, |(w, _)| w).min(bounds.width());
    let floating = crate::tool::is_floating(ctx, PROFILER_ID);
    let limit = if floating {
        ctx.content_rect().shrink(edge)
    } else {
        bounds
    };
    let placed = |pos| {
        crate::tool::placed_rect(
            ctx,
            PROFILER_ID,
            egui::Align2::RIGHT_TOP,
            pos,
            egui::vec2(width, 0.0),
            bounds,
            // (Its height follows its contents; its width is set here every frame.)
            false,
        )
    };
    let mut dock_pos = bounds.right_top();
    let mut profiler = placed(dock_pos);
    // The overlay is docked at the top-left corner (its size from the last frame).
    let overlay = ctx
        .memory(|m| m.area_rect(egui::Id::new(crate::debug::OVERLAY_ID)))
        .filter(|_| overlay_open)
        .map(|r| egui::Rect::from_min_size(bounds.left_top(), r.size()));
    // (Beside the console, the console keeps left of it.)
    let console = (console_open && plan.profiler_beside.is_none()).then(|| crate::console::placed_rect(ctx, plan));
    let below = |profiler: egui::Rect, console: egui::Rect| {
        console.x_range().intersects(profiler.x_range()) && console.top() > profiler.top()
    };
    let mut max_width = plan.profiler_beside.map_or(f32::INFINITY, |(_, max)| max);
    if let Some(overlay) = overlay
        && plan.profiler_beside.is_none()
        && profiler.top() < overlay.bottom() + edge
    {
        let room = profiler.right() - overlay.right() - edge;
        if room >= MIN_WIDTH {
            max_width = room;
        } else if !floating {
            let stacked = placed(egui::pos2(bounds.right(), overlay.bottom() + edge));
            let floor = console
                .filter(|&c| below(stacked, c))
                .map_or(bounds.bottom(), |c| c.top() - edge);
            if floor - stacked.top() >= MIN_STACKED_HEIGHT {
                dock_pos = stacked.right_top();
                profiler = stacked;
            }
        }
    }
    let mut height = (dock.height() * 0.45).max(300.0).min(limit.bottom() - profiler.top());
    if let Some(console) = console
        && below(profiler, console)
    {
        height = height.min(console.top() - edge - profiler.top());
    }
    Placement {
        dock_pos,
        bounds,
        size: egui::vec2(width.min(max_width), height.max(90.0)),
        max_width,
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

    /// A profiler with `n` systems of distinct timings.
    fn profiler_with(n: u32) -> Profiler {
        let mut p = Profiler::default();
        for frame in 0..8u64 {
            let spans = (0..n)
                .map(|i| TimelineSpan {
                    name: intern(&format!("game::system_{i}")),
                    start: f64::from(i) * 0.0001,
                    duration: 0.0001 * f64::from(n - i) * (1.0 + 0.1 * frame as f64),
                    depth: 0,
                })
                .collect();
            p.record(FrameTimeline {
                index: frame + 1,
                duration: 0.004,
                spans,
            });
        }
        p
    }

    fn texts(shape: &egui::Shape, out: &mut Vec<(String, egui::Rect)>) {
        match shape {
            egui::Shape::Text(t) => out.push((t.galley.text().to_owned(), t.galley.rect.translate(t.pos.to_vec2()))),
            egui::Shape::Vec(v) => v.iter().for_each(|s| texts(s, out)),
            _ => {}
        }
    }

    /// The texts painted by the profiler shown in a docked tool window `size` big, and the
    /// window's rect.
    fn painted(p: &mut Profiler, size: egui::Vec2) -> (Vec<(String, egui::Rect)>, egui::Rect) {
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 720.0));
        let mut shapes = Vec::new();
        for _ in 0..4 {
            let raw = egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            };
            let mut out = ctx.run_ui(raw, |ui| {
                let ctx = ui.ctx().clone();
                let mut open = true;
                let fit = crate::tool::window(
                    &ctx,
                    crate::tool::ToolWindow {
                        id: PROFILER_ID,
                        title: "Profiler",
                        anchor: egui::Align2::RIGHT_TOP,
                        dock_pos: screen.shrink(6.0).right_top(),
                        bounds: screen.shrink(6.0),
                        size,
                        fit_height: true,
                        max_width: f32::INFINITY,
                    },
                    &mut open,
                    |_| {},
                    |ui, max_bottom| p.ui_until(ui, max_bottom),
                );
                if let Some(Some(fit)) = fit {
                    p.fit = fit;
                }
            });
            shapes = out.shapes;
            out.textures_delta.clear();
        }
        let mut all = Vec::new();
        for s in &shapes {
            texts(&s.shape, &mut all);
        }
        let window = ctx.memory(|m| m.area_rect(egui::Id::new(PROFILER_ID))).expect("shown");
        (all, window)
    }

    /// Table rows shown in `window` (each with 3 numbers with a decimal point; egui paints a
    /// row below the scroll area too, clipped).
    fn rows(texts: &[(String, egui::Rect)], window: egui::Rect) -> usize {
        let numbers = texts
            .iter()
            .filter(|(t, r)| t.contains('.') && t.parse::<f64>().is_ok() && r.bottom() <= window.bottom());
        numbers.count() / 3
    }

    /// Right edges of the table's header titles and of the numbers painted in its rows (one
    /// entry per distinct column), and how many rows were painted, for the profiler shown in a
    /// docked tool window `size` big.
    fn table_columns(p: &mut Profiler, size: egui::Vec2) -> (Vec<(String, f32)>, Vec<f32>, usize) {
        let (all, _) = painted(p, size);
        let headers = all
            .iter()
            .filter(|(t, _)| ["avg", "last", "max", "calls"].contains(&t.as_str()))
            .map(|(t, r)| (t.clone(), r.right()))
            .collect();
        let numbers: Vec<&(String, egui::Rect)> = all.iter().filter(|(t, _)| t.parse::<f64>().is_ok()).collect();
        let rows = numbers.iter().filter(|(t, _)| t.contains('.')).count();
        let mut columns: Vec<f32> = Vec::new();
        for (_, r) in numbers {
            if !columns.iter().any(|&x| (x - r.right()).abs() < 0.5) {
                columns.push(r.right());
            }
        }
        (headers, columns, rows)
    }

    /// A window too short for the sections and 3 table rows leaves the section headers out
    /// (the flame and the table stay); `min_height` is the height of that layout with n rows.
    #[test]
    fn a_short_profiler_leaves_out_its_section_headers() {
        let ctx = egui::Context::default();
        for (height, headers, min_rows) in [(168.0, false, 4), (189.0, false, 5), (190.0, true, 3), (221.0, true, 4)] {
            let mut p = profiler_with(14);
            let (texts, window) = painted(&mut p, egui::vec2(440.0, height));
            let shown = rows(&texts, window);
            let label = format!("{height} pt: {window:?}, {shown} rows");
            let sections = ["Timeline", "Systems (14)"].map(|h| texts.iter().any(|(t, _)| t == h));
            assert_eq!(sections, [headers; 2], "section headers: {label}");
            assert!(shown >= min_rows, "{label}");
            assert!(window.height() <= height + 0.5, "{label}");
        }
        // The measured minimum: exactly n whole rows in the compact layout.
        let mut p = profiler_with(14);
        let _ = painted(&mut p, egui::vec2(440.0, 400.0));
        for n in [1, 3] {
            let min = p.min_height(&ctx, n);
            let (texts, window) = painted(&mut p, egui::vec2(440.0, min));
            assert_eq!(rows(&texts, window), n, "{min}: {window:?}");
            assert!((window.height() - min).abs() < 0.5, "{min}: {window:?}");
        }
        assert!(
            p.min_height(&ctx, 3) <= 150.0,
            "a small minimum: {}",
            p.min_height(&ctx, 3)
        );
    }

    /// The header titles sit right above their right-aligned numbers, whether the table
    /// scrolls (its scroll bar takes a column of its own) or not; a narrow table keeps avg and
    /// calls only.
    #[test]
    fn table_headers_line_up_with_their_columns() {
        for (systems, size, scrolls, titles) in [
            (40, egui::vec2(440.0, 320.0), true, 4),
            (6, egui::vec2(440.0, 600.0), false, 4),
            (40, egui::vec2(250.0, 320.0), true, 2),
        ] {
            let mut p = profiler_with(systems);
            let (headers, mut columns, rows) = table_columns(&mut p, size);
            let label = format!("{systems} systems in {size:?}: headers {headers:?}, columns {columns:?}");
            assert_eq!(rows / (titles - 1) < systems as usize, scrolls, "scrolling: {label}");
            assert_eq!(headers.len(), titles, "{label}");
            columns.sort_by(f32::total_cmp);
            assert_eq!(columns.len(), titles, "{label}");
            let mut edges: Vec<f32> = headers.iter().map(|(_, x)| *x).collect();
            edges.sort_by(f32::total_cmp);
            for (h, c) in edges.iter().zip(&columns) {
                assert!((h - c).abs() < 0.5, "header {h} over column {c}: {label}");
            }
        }
    }
}
