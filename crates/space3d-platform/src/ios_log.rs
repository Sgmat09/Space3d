//! iOS: log output that reaches the unified log.
//!
//! An iOS app's stderr is only visible when a debugger or `xcrun simctl launch --console-pty`
//! is attached; launched from the home screen it goes nowhere. [`IosLogWriter`] (the writer the
//! engine's `tracing` subscribers use on iOS) writes every formatted line to stderr **and** to
//! the unified log, subsystem [`IOS_LOG_SUBSYSTEM`], so `log stream`, `log show` and
//! Console.app show it:
//!
//! ```text
//! xcrun simctl spawn <device> log stream --level info --predicate 'subsystem == "space3d"'
//! ```
//!
//! [`install_panic_hook`] sends Rust panics there too (as faults), before the default hook
//! prints them to stderr.

use std::io::Write;
use std::sync::OnceLock;
use tracing_subscriber::fmt::MakeWriter;

/// Unified-log subsystem of the engine's log lines (category `log`; panics: `panic`).
pub const IOS_LOG_SUBSYSTEM: &str = "space3d";

fn os_log(category: &'static str) -> &'static oslog::OsLog {
    static LOG: OnceLock<oslog::OsLog> = OnceLock::new();
    static PANIC: OnceLock<oslog::OsLog> = OnceLock::new();
    let cell = if category == "panic" { &PANIC } else { &LOG };
    cell.get_or_init(|| oslog::OsLog::new(IOS_LOG_SUBSYSTEM, category))
}

/// Unified-log level for a `tracing` level. Info is logged at the *default* level, which the
/// system persists (so `log show` finds it later); debug/trace stay in memory.
fn level_for(level: &tracing::Level) -> oslog::Level {
    match *level {
        tracing::Level::ERROR => oslog::Level::Error,
        tracing::Level::WARN | tracing::Level::INFO => oslog::Level::Default,
        tracing::Level::DEBUG => oslog::Level::Info,
        tracing::Level::TRACE => oslog::Level::Debug,
    }
}

/// `tracing_subscriber::fmt` writer for iOS: stderr plus the unified log (see the module docs).
/// Use it with `.with_ansi(false)` (neither Xcode's console nor the unified log render colors).
#[derive(Clone, Copy, Debug, Default)]
pub struct IosLogWriter;

/// One formatted event, sent when dropped.
#[derive(Debug)]
pub struct IosLogLine {
    buf: Vec<u8>,
    level: tracing::Level,
}

impl Write for IosLogLine {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for IosLogLine {
    fn drop(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        let _ = std::io::stderr().write_all(&self.buf);
        let text = String::from_utf8_lossy(&self.buf);
        os_log("log").with_level(level_for(&self.level), text.trim_end());
    }
}

impl<'a> MakeWriter<'a> for IosLogWriter {
    type Writer = IosLogLine;

    fn make_writer(&'a self) -> IosLogLine {
        IosLogLine {
            buf: Vec::new(),
            level: tracing::Level::INFO,
        }
    }

    fn make_writer_for(&'a self, meta: &tracing::Metadata<'_>) -> IosLogLine {
        IosLogLine {
            buf: Vec::new(),
            level: *meta.level(),
        }
    }
}

/// Log Rust panics to the unified log (category `panic`, fault level) before the previous hook
/// runs (the default one prints to stderr). Idempotent.
pub fn install_panic_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let thread = std::thread::current();
            let name = thread.name().unwrap_or("<unnamed>");
            os_log("panic").with_level(oslog::Level::Fault, &format!("thread '{name}' {info}"));
            previous(info);
        }));
    });
}
