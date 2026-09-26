//! In-game console: `tracing` log capture ([`ConsoleLayer`]) and a command registry
//! ([`Console`]).
//!
//! ```
//! use space3d_core::prelude::*;
//! use space3d_ui::Console;
//!
//! let mut console = Console::with_builtin_commands();
//! console.register("spawn", "spawn <n>: spawn n empty entities", |args, sys| {
//!     let n: usize = args.first().and_then(|a| a.parse().ok()).ok_or("usage: spawn <n>")?;
//!     for _ in 0..n {
//!         sys.world.spawn((Transform::default(),));
//!     }
//!     Ok(format!("spawned {n}"))
//! });
//! let mut app = App::new();
//! app.insert_resource(console);
//! app.resources.expect_mut::<Console>().submit("spawn 3");
//! app.add_system(Stage::Update, |sys| Console::run_pending(sys));
//! app.step_ticks(1);
//! assert_eq!(app.world.len(), 3);
//! assert!(app.resources.expect::<Console>().lines().any(|l| l.message == "spawned 3"));
//! ```

use space3d_core::{AppExit, SystemCtx, TickRate, Time};
use std::collections::{BTreeMap, VecDeque};
use tracing::Subscriber;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

/// Severity of a console line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LogLevel {
    /// `trace!`.
    Trace,
    /// `debug!`.
    Debug,
    /// `info!`.
    Info,
    /// `warn!`.
    Warn,
    /// `error!`.
    Error,
    /// A command typed into the console.
    Command,
    /// Output of a command.
    Output,
}

impl LogLevel {
    fn from_tracing(l: &tracing::Level) -> Self {
        match *l {
            tracing::Level::TRACE => Self::Trace,
            tracing::Level::DEBUG => Self::Debug,
            tracing::Level::INFO => Self::Info,
            tracing::Level::WARN => Self::Warn,
            tracing::Level::ERROR => Self::Error,
        }
    }

    fn color(self, visuals: &egui::Visuals) -> egui::Color32 {
        match self {
            Self::Trace | Self::Debug => visuals.weak_text_color(),
            Self::Info => visuals.text_color(),
            Self::Warn => visuals.warn_fg_color,
            Self::Error => visuals.error_fg_color,
            Self::Command => egui::Color32::from_rgb(110, 170, 255),
            Self::Output => egui::Color32::from_rgb(150, 220, 150),
        }
    }

    fn tag(self) -> &'static str {
        match self {
            Self::Trace => "TRACE",
            Self::Debug => "DEBUG",
            Self::Info => " INFO",
            Self::Warn => " WARN",
            Self::Error => "ERROR",
            Self::Command => "   > ",
            Self::Output => "     ",
        }
    }
}

/// One console line.
#[derive(Clone, Debug, PartialEq)]
pub struct LogLine {
    /// Global sequence number.
    pub seq: u64,
    /// Severity.
    pub level: LogLevel,
    /// Event target (module path) or `"console"`.
    pub target: String,
    /// Message plus `key=value` fields.
    pub message: String,
}

const GLOBAL_CAPACITY: usize = 2048;
const LOCAL_CAPACITY: usize = 1000;

struct LogBuffer {
    next_seq: u64,
    lines: VecDeque<LogLine>,
}

static LOG: parking_lot::Mutex<LogBuffer> = parking_lot::Mutex::new(LogBuffer {
    next_seq: 0,
    lines: VecDeque::new(),
});

fn push_global(level: LogLevel, target: &str, message: String) {
    let mut log = LOG.lock();
    let seq = log.next_seq;
    log.next_seq += 1;
    if log.lines.len() == GLOBAL_CAPACITY {
        log.lines.pop_front();
    }
    log.lines.push_back(LogLine {
        seq,
        level,
        target: target.to_owned(),
        message,
    });
}

#[derive(Default)]
struct MessageVisitor {
    message: String,
    fields: String,
}

impl tracing::field::Visit for MessageVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            self.fields.push_str(&format!(" {}={value}", field.name()));
        }
    }
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message.push_str(&format!("{value:?}"));
        } else if !field.name().starts_with("log.") {
            self.fields.push_str(&format!(" {}={value:?}", field.name()));
        }
    }
}

/// `tracing` layer copying events into the console log buffer (process-wide ring buffer of
/// the last 2048 lines). Installed by [`crate::install_tracing`].
#[derive(Clone, Copy, Debug, Default)]
pub struct ConsoleLayer {
    _private: (),
}

impl ConsoleLayer {
    /// New layer.
    pub fn new() -> Self {
        Self::default()
    }
}

impl<S: Subscriber> Layer<S> for ConsoleLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut v = MessageVisitor::default();
        event.record(&mut v);
        let meta = event.metadata();
        v.message.push_str(&v.fields);
        push_global(LogLevel::from_tracing(meta.level()), meta.target(), v.message);
    }
}

type CommandFn = Box<dyn FnMut(&[&str], &mut SystemCtx) -> Result<String, String> + Send + Sync>;

/// A registered console command.
pub struct ConsoleCommand {
    /// One-line help text.
    pub help: String,
    run: CommandFn,
}

impl std::fmt::Debug for ConsoleCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConsoleCommand").field("help", &self.help).finish()
    }
}

/// In-game console (resource inserted by [`crate::UiPlugin`]; toggle with the backquote key).
///
/// Shows captured log lines and runs commands: `help`, `clear`, `echo`, `entities`,
/// `tickrate [hz]`, `time`, `debug`, `profiler`, `stats`, `exit` plus anything added with
/// [`Console::register`]. Commands submitted with [`Console::submit`] run during the next
/// `Stage::Update` with full world/resource access.
pub struct Console {
    /// Show the console window.
    pub visible: bool,
    /// Lowest level shown.
    pub min_level: LogLevel,
    /// Case-insensitive text filter.
    pub filter: String,
    /// Copy lines from the global log buffer ([`ConsoleLayer`]) every frame (default `true`).
    /// Off, the console holds only its own lines: command output and [`Console::print`].
    pub capture_logs: bool,
    lines: VecDeque<LogLine>,
    last_seq: Option<u64>,
    local_seq: u64,
    input: String,
    history: Vec<String>,
    history_pos: Option<usize>,
    commands: BTreeMap<String, ConsoleCommand>,
    pending: Vec<String>,
    focus_input: bool,
    input_focused: bool,
}

impl std::fmt::Debug for Console {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Console")
            .field("visible", &self.visible)
            .field("lines", &self.lines.len())
            .field("commands", &self.commands.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Default for Console {
    fn default() -> Self {
        Self {
            visible: false,
            min_level: LogLevel::Info,
            filter: String::new(),
            capture_logs: true,
            lines: VecDeque::new(),
            last_seq: None,
            local_seq: u64::MAX / 2,
            input: String::new(),
            history: Vec::new(),
            history_pos: None,
            commands: BTreeMap::new(),
            pending: Vec::new(),
            focus_input: false,
            input_focused: false,
        }
    }
}

impl Console {
    /// Console with the built-in commands registered.
    pub fn with_builtin_commands() -> Self {
        let mut c = Self::default();
        c.register("echo", "echo <text>: print text", |args, _| Ok(args.join(" ")));
        c.register("entities", "entities: number of live entities", |_, sys| {
            Ok(format!("{} entities", sys.world.len()))
        });
        c.register(
            "tickrate",
            "tickrate [hz]: show or set the fixed tick rate",
            |args, sys| match args.first() {
                None => Ok(format!("{} Hz", sys.resources.expect::<TickRate>().hz)),
                Some(a) => {
                    let hz: f64 = a.parse().map_err(|_| format!("not a number: {a}"))?;
                    if hz <= 0.0 || !hz.is_finite() {
                        return Err("tick rate must be positive".into());
                    }
                    let mut rate = *sys.resources.expect::<TickRate>();
                    rate.hz = hz;
                    sys.resources.insert(rate);
                    sys.resources.expect_mut::<Time>().fixed_delta = rate.step();
                    Ok(format!("tick rate set to {hz} Hz"))
                }
            },
        );
        c.register("time", "time: frame, tick and elapsed time", |_, sys| {
            let t = sys.resources.expect::<Time>();
            Ok(format!(
                "frame {} · tick {} · {:.2}s elapsed · {} dropped ticks",
                t.frame,
                t.tick,
                t.elapsed_secs(),
                t.dropped_ticks
            ))
        });
        c.register(
            "debug",
            "debug [details]: toggle the debug overlay (F3), or its renderer details",
            |args, sys| {
                let d = sys
                    .resources
                    .get_mut::<crate::DebugOverlay>()
                    .ok_or("no debug overlay")?;
                let on_off = |b: bool| if b { "on" } else { "off" };
                match args.first().copied() {
                    None => {
                        d.visible = !d.visible;
                        Ok(format!("debug overlay {}", on_off(d.visible)))
                    }
                    Some("details") => {
                        d.details = !d.details;
                        d.visible = true;
                        Ok(format!("debug overlay details {}", on_off(d.details)))
                    }
                    Some(other) => Err(format!("unknown argument `{other}` (usage: debug [details])")),
                }
            },
        );
        c.register("profiler", "profiler: toggle the profiler (F4)", |_, sys| {
            let p = sys.resources.get_mut::<crate::Profiler>().ok_or("no profiler")?;
            p.visible = !p.visible;
            Ok(format!("profiler {}", if p.visible { "on" } else { "off" }))
        });
        c.register("stats", "stats: renderer statistics of the last frame", |_, sys| {
            let s = sys
                .resources
                .get::<space3d_render::RenderStats>()
                .ok_or("no renderer")?;
            Ok(format!(
                "{:?}: {} draws ({} indirect), {} instances, {} triangles, {} bytes uploaded",
                s.draw_path, s.draw_calls, s.indirect_draw_calls, s.instances_drawn, s.triangles, s.bytes_uploaded
            ))
        });
        c.register("exit", "exit: quit the app", |_, sys| {
            sys.send(AppExit);
            Ok("bye".into())
        });
        c
    }

    /// Register (or replace) a command. `run` gets the whitespace-separated arguments and the
    /// system context; `Ok` text is printed as output, `Err` as an error.
    pub fn register(
        &mut self,
        name: impl Into<String>,
        help: impl Into<String>,
        run: impl FnMut(&[&str], &mut SystemCtx) -> Result<String, String> + Send + Sync + 'static,
    ) -> &mut Self {
        self.commands.insert(
            name.into(),
            ConsoleCommand {
                help: help.into(),
                run: Box::new(run),
            },
        );
        self
    }

    /// Remove a command.
    pub fn unregister(&mut self, name: &str) -> bool {
        self.commands.remove(name).is_some()
    }

    /// Registered command names (sorted).
    pub fn command_names(&self) -> impl Iterator<Item = &str> {
        self.commands.keys().map(String::as_str)
    }

    /// Queue a command line; it runs in the next `Stage::Update` ([`Console::run_pending`]).
    pub fn submit(&mut self, line: impl Into<String>) {
        let line = line.into();
        if !line.trim().is_empty() {
            self.pending.push(line);
        }
    }

    /// Print a line into this console (not the global log).
    pub fn print(&mut self, level: LogLevel, message: impl Into<String>) {
        self.local_seq += 1;
        let line = LogLine {
            seq: self.local_seq,
            level,
            target: "console".into(),
            message: message.into(),
        };
        self.push_line(line);
    }

    fn push_line(&mut self, line: LogLine) {
        if self.lines.len() == LOCAL_CAPACITY {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    /// Lines currently held (captured logs and command output), oldest first.
    pub fn lines(&self) -> impl Iterator<Item = &LogLine> {
        self.lines.iter()
    }

    /// Remove all lines.
    pub fn clear(&mut self) {
        self.lines.clear();
    }

    /// Copy new lines from the global log buffer.
    pub fn pull_logs(&mut self) {
        let new: Vec<LogLine> = {
            let log = LOG.lock();
            let from = self.last_seq.map_or(0, |s| s + 1);
            log.lines.iter().filter(|l| l.seq >= from).cloned().collect()
        };
        if let Some(last) = new.last() {
            self.last_seq = Some(last.seq);
        }
        for l in new {
            self.push_line(l);
        }
    }

    /// Run queued commands (called by the plugin every frame; call it yourself when using
    /// the console without [`crate::UiPlugin`]).
    pub fn run_pending(sys: &mut SystemCtx) {
        let Some(mut console) = sys.resources.remove::<Console>() else {
            return;
        };
        for line in std::mem::take(&mut console.pending) {
            console.execute(&line, sys);
        }
        sys.resources.insert(console);
    }

    fn execute(&mut self, line: &str, sys: &mut SystemCtx) {
        self.print(LogLevel::Command, line);
        if self.history.last().is_none_or(|l| l != line) {
            self.history.push(line.to_owned());
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        let Some((&name, args)) = parts.split_first() else {
            return;
        };
        match name {
            "help" => {
                let mut out = vec!["help: list commands".to_owned(), "clear: clear the console".to_owned()];
                out.extend(self.commands.values().map(|c| c.help.clone()));
                for l in out {
                    self.print(LogLevel::Output, l);
                }
            }
            "clear" => self.clear(),
            _ => match self.commands.get_mut(name) {
                Some(cmd) => match (cmd.run)(args, sys) {
                    Ok(out) => {
                        for l in out.lines() {
                            self.print(LogLevel::Output, l);
                        }
                    }
                    Err(e) => self.print(LogLevel::Error, e),
                },
                None => self.print(LogLevel::Error, format!("unknown command `{name}` (try `help`)")),
            },
        }
    }

    /// Toggle visibility (focuses the input line when opening).
    pub fn toggle(&mut self) {
        self.visible = !self.visible;
        self.focus_input = self.visible;
        if !self.visible {
            self.input_focused = false;
        }
    }

    /// Whether the command input line had keyboard focus in the last drawn frame.
    pub fn input_has_focus(&self) -> bool {
        self.input_focused
    }

    /// Lines shown with the current level and text filter.
    fn shown(&self) -> impl Iterator<Item = &LogLine> {
        let filter = self.filter.to_lowercase();
        self.lines.iter().filter(move |l| {
            let level = match l.level {
                LogLevel::Command | LogLevel::Output => true,
                lvl => lvl >= self.min_level,
            };
            level && (filter.is_empty() || l.message.to_lowercase().contains(&filter))
        })
    }

    /// Draw the console into `ui`: a toolbar (level, filter, clear), the log (one dense
    /// monospace line per entry) and the input line.
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("space3d_console_level")
                .selected_text(format!("{:?}", self.min_level))
                .width(64.0)
                .show_ui(ui, |ui| {
                    for l in [
                        LogLevel::Trace,
                        LogLevel::Debug,
                        LogLevel::Info,
                        LogLevel::Warn,
                        LogLevel::Error,
                    ] {
                        ui.selectable_value(&mut self.min_level, l, format!("{l:?}"));
                    }
                });
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text("filter")
                    .desired_width(140.0),
            );
            if ui.button("Clear").clicked() {
                self.clear();
            }
        });
        let visuals = ui.visuals().clone();
        let font = egui::TextStyle::Monospace.resolve(ui.style());
        let input_height = ui.spacing().interact_size.y + ui.spacing().item_spacing.y + 2.0;
        egui::Frame::new()
            .fill(visuals.extreme_bg_color)
            .corner_radius(2)
            .inner_margin(egui::Margin::symmetric(4, 2))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("space3d_console_log")
                    .max_height((ui.available_height() - input_height - 4.0).max(48.0))
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        for l in self.shown() {
                            let color = l.level.color(&visuals);
                            let mut job = egui::text::LayoutJob::default();
                            let format = |c| egui::TextFormat::simple(font.clone(), c);
                            match l.level {
                                LogLevel::Command | LogLevel::Output => {
                                    job.append(l.level.tag(), 0.0, format(color));
                                    job.append(&l.message, 0.0, format(color));
                                }
                                _ => {
                                    job.append(l.level.tag(), 0.0, format(color));
                                    job.append(&format!(" {}: ", l.target), 0.0, format(visuals.weak_text_color()));
                                    job.append(&l.message, 0.0, format(color));
                                }
                            }
                            ui.label(job);
                        }
                    });
            });
        let edit = egui::TextEdit::singleline(&mut self.input)
            .hint_text("command (try `help`)")
            .desired_width(f32::INFINITY)
            .font(egui::TextStyle::Monospace);
        let response = ui.add(edit);
        if self.focus_input {
            response.request_focus();
            self.focus_input = false;
        }
        if response.has_focus() {
            let (up, down) = ui.input(|i| (i.key_pressed(egui::Key::ArrowUp), i.key_pressed(egui::Key::ArrowDown)));
            if up && !self.history.is_empty() {
                let pos = self.history_pos.map_or(self.history.len() - 1, |p| p.saturating_sub(1));
                self.history_pos = Some(pos);
                self.input = self.history[pos].clone();
            }
            if down && let Some(p) = self.history_pos {
                if p + 1 < self.history.len() {
                    self.history_pos = Some(p + 1);
                    self.input = self.history[p + 1].clone();
                } else {
                    self.history_pos = None;
                    self.input.clear();
                }
            }
        }
        if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            let line = std::mem::take(&mut self.input).replace('`', "");
            self.submit(line);
            self.history_pos = None;
            response.request_focus();
        }
        self.input_focused = response.has_focus() || self.focus_input;
    }
}

/// `Stage::Update`: pull logs, run queued commands and draw the console window.
pub(crate) fn console_system(sys: &mut SystemCtx) {
    let ctx = crate::EguiContext::active_context(sys);
    if let Some(c) = sys.resources.get_mut::<Console>()
        && c.capture_logs
    {
        c.pull_logs();
    }
    if let Some(ctx) = ctx
        && let Some(c) = sys.resources.get_mut::<Console>()
        && c.visible
    {
        let mut open = true;
        let screen = ctx.content_rect();
        let size = egui::vec2(
            (screen.width() - 12.0).min(640.0),
            (screen.height() * 0.4).clamp(140.0, 360.0).min(screen.height() - 12.0),
        );
        let pos = egui::pos2(6.0, (screen.bottom() - size.y - 6.0).max(6.0));
        let (shown, total) = (c.shown().count(), c.lines.len());
        crate::tool::window(
            &ctx,
            ("space3d_console", "Console"),
            egui::Rect::from_min_size(pos, size),
            &mut open,
            |ui| {
                ui.label(egui::RichText::new(format!("{shown}/{total} lines")).weak());
            },
            |ui| c.ui(ui),
        );
        if !open {
            c.visible = false;
        }
    }
    if let Some(c) = sys.resources.get_mut::<Console>()
        && !c.visible
    {
        c.input_focused = false;
    }
    Console::run_pending(sys);
}

#[cfg(test)]
mod tests {
    use super::*;
    use space3d_core::App;

    #[test]
    fn commands_run_with_world_access() {
        let mut app = App::new();
        let mut c = Console::with_builtin_commands();
        c.register("fail", "fail", |_, _| Err("boom".into()));
        c.submit("tickrate 30");
        c.submit("fail");
        c.submit("nope");
        c.submit("echo hello world");
        c.submit("debug details");
        app.insert_resource(c);
        app.insert_resource(crate::DebugOverlay::new(false));
        app.add_system(space3d_core::Stage::Update, Console::run_pending);
        app.step_ticks(1);
        assert_eq!(app.resources.expect::<TickRate>().hz, 30.0);
        let c = app.resources.expect::<Console>();
        let msgs: Vec<(LogLevel, &str)> = c.lines().map(|l| (l.level, l.message.as_str())).collect();
        assert!(msgs.contains(&(LogLevel::Error, "boom")));
        assert!(msgs.contains(&(LogLevel::Output, "hello world")));
        assert!(
            msgs.iter()
                .any(|(l, m)| *l == LogLevel::Error && m.contains("unknown command"))
        );
        assert_eq!(c.history.len(), 5);
        let overlay = app.resources.expect::<crate::DebugOverlay>();
        assert!(overlay.visible && overlay.details, "`debug details` shows the details");
    }

    #[test]
    fn layer_captures_events() {
        use tracing_subscriber::layer::SubscriberExt;
        let subscriber = tracing_subscriber::registry().with(ConsoleLayer::new());
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(answer = 42, "captured-by-console-layer");
        });
        let mut c = Console::default();
        c.pull_logs();
        let line = c
            .lines()
            .find(|l| l.message.contains("captured-by-console-layer"))
            .expect("line captured");
        assert_eq!(line.level, LogLevel::Warn);
        assert!(line.message.contains("answer=42"));
    }
}
