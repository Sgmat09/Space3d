//! Browser-console logging for the web.
//!
//! [`ConsoleLayer`] (wasm32) writes every `tracing` event with the console method of its level
//! — `console.error`, `console.warn`, `console.info`, `console.debug` (debug and trace) — so the
//! browser's level filter and error count, and automated checks such as
//! `scripts/web-smoke.mjs`, see engine errors as errors. [`init_web_logging`] /
//! [`try_init_web_logging`] install it as the global subscriber (with a panic hook that sends
//! Rust panics to `console.error`); `space3d::LogPlugin` and `space3d_ui::UiPlugin` do that on
//! the web.
//!
//! The line format ([`format_event`]) is platform independent: `LEVEL target: message
//! key=value …`.

use std::fmt::Write as _;
use tracing::Level;
use tracing::field::{Field, Visit};

/// Default filter directives (`tracing_subscriber::EnvFilter` syntax) for the browser console:
/// the same as `space3d::LogPlugin`'s.
pub const DEFAULT_WEB_FILTER: &str = "info,wgpu_core=warn,wgpu_hal=warn,naga=warn,egui_wgpu=error";

/// The `console` method an event of `level` is written with.
pub fn console_method(level: Level) -> &'static str {
    match level {
        Level::ERROR => "error",
        Level::WARN => "warn",
        Level::INFO => "info",
        _ => "debug",
    }
}

/// `LEVEL target: message key=value …` for an event. A `log` record bridged by `tracing-log`
/// is shown under its own target, without the `log.*` bookkeeping fields.
pub fn format_event(event: &tracing::Event<'_>) -> String {
    #[derive(Default)]
    struct Fields {
        message: String,
        target: Option<String>,
        rest: String,
    }
    impl Fields {
        fn record(&mut self, field: &Field, value: std::fmt::Arguments<'_>) {
            match field.name() {
                "message" => {
                    let _ = self.message.write_fmt(value);
                }
                "log.target" => self.target = Some(value.to_string()),
                name if name.starts_with("log.") => {}
                name => {
                    let _ = write!(self.rest, " {name}={value}");
                }
            }
        }
    }
    impl Visit for Fields {
        fn record_str(&mut self, field: &Field, value: &str) {
            self.record(field, format_args!("{value}"));
        }
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.record(field, format_args!("{value:?}"));
        }
    }
    let mut fields = Fields::default();
    event.record(&mut fields);
    let meta = event.metadata();
    let target = fields.target.as_deref().unwrap_or(meta.target());
    format!("{} {target}: {}{}", meta.level(), fields.message, fields.rest)
}

#[cfg(target_arch = "wasm32")]
pub use web::{ConsoleLayer, init_web_logging, try_init_web_logging};

#[cfg(target_arch = "wasm32")]
mod web {
    use super::{DEFAULT_WEB_FILTER, format_event};
    use tracing::{Level, Subscriber};
    use tracing_subscriber::layer::{Context, SubscriberExt};
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::{EnvFilter, Layer};
    use wasm_bindgen::JsValue;
    use web_sys::console;

    /// `tracing` layer writing events to the browser console at their level (see the module
    /// docs). Filter it like any layer (`ConsoleLayer.with_filter(EnvFilter::new("info"))`).
    #[derive(Clone, Copy, Debug, Default)]
    pub struct ConsoleLayer;

    impl<S: Subscriber> Layer<S> for ConsoleLayer {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            let text = JsValue::from_str(&format_event(event));
            match *event.metadata().level() {
                Level::ERROR => console::error_1(&text),
                Level::WARN => console::warn_1(&text),
                Level::INFO => console::info_1(&text),
                _ => console::debug_1(&text),
            }
        }
    }

    /// Route panics to `console.error` and install [`ConsoleLayer`] filtered by `filter`
    /// (`EnvFilter` directives; [`DEFAULT_WEB_FILTER`] if they don't parse) as the global
    /// subscriber, bridging `log` records. Returns `false` when another global subscriber was
    /// already installed (it stays). Safe to call more than once.
    pub fn try_init_web_logging(filter: &str) -> bool {
        console_error_panic_hook::set_once();
        let filter = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new(DEFAULT_WEB_FILTER));
        tracing_subscriber::registry()
            .with(ConsoleLayer.with_filter(filter))
            .try_init()
            .is_ok()
    }

    /// [`try_init_web_logging`] with [`DEFAULT_WEB_FILTER`].
    pub fn init_web_logging() {
        try_init_web_logging(DEFAULT_WEB_FILTER);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::{Context, SubscriberExt};

    struct Capture(Arc<Mutex<Vec<(Level, String)>>>);

    impl<S: tracing::Subscriber> Layer<S> for Capture {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            self.0
                .lock()
                .unwrap()
                .push((*event.metadata().level(), format_event(event)));
        }
    }

    #[test]
    fn events_become_console_lines() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(Capture(lines.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!(target: "space3d_render", "failed to create renderer: {}", "no adapter");
            tracing::warn!(dropped = 7, "simulation falling behind");
            tracing::info!(adapter = %"ANGLE", backend = ?Some(1), "space3d renderer initialised");
            // What `tracing-log` produces for a bridged `log` record.
            tracing::warn!(
                log.target = "egui_wgpu::renderer",
                log.module_path = "egui_wgpu::renderer",
                log.file = "renderer.rs",
                log.line = 410,
                "Detected a linear framebuffer"
            );
            tracing::debug!(message = "plain");
        });
        let lines = lines.lock().unwrap();
        let text: Vec<&str> = lines.iter().map(|(_, l)| l.as_str()).collect();
        assert_eq!(
            text,
            [
                "ERROR space3d_render: failed to create renderer: no adapter",
                &format!("WARN {}: simulation falling behind dropped=7", module_path!()),
                &format!(
                    "INFO {}: space3d renderer initialised adapter=ANGLE backend=Some(1)",
                    module_path!()
                ),
                "WARN egui_wgpu::renderer: Detected a linear framebuffer",
                &format!("DEBUG {}: plain", module_path!()),
            ]
        );
        let methods: Vec<&str> = lines.iter().map(|(l, _)| console_method(*l)).collect();
        assert_eq!(methods, ["error", "warn", "info", "warn", "debug"]);
        assert_eq!(console_method(Level::TRACE), "debug");
    }

    #[test]
    fn default_filter_parses() {
        assert!(tracing_subscriber::EnvFilter::try_new(DEFAULT_WEB_FILTER).is_ok());
    }
}
