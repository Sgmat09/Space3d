//! The directives of the engine's log filters (`space3d::LogPlugin`, `space3d_ui`'s subscriber).

/// The target of winit's iOS event-order messages: about four lines per frame (warnings
/// "processing non `RedrawRequested` event after the main event loop: AboutToWait", info lines
/// "processing `RedrawRequested` during the main event loop").
pub const WINIT_IOS_EVENT_ORDER_TARGET: &str = "winit::platform_impl::ios::app_state";

/// `tracing_subscriber::EnvFilter` directives for the engine's log output: `rust_log` (the
/// `RUST_LOG` value) when it is set and not blank, else `default`. On iOS, errors only for
/// [`WINIT_IOS_EVENT_ORDER_TARGET`] are appended unless the directives name that target, so
/// `RUST_LOG=info` or `debug` doesn't flood stderr and the unified log with winit's per-frame
/// lines (`RUST_LOG=info,winit::platform_impl::ios::app_state=info` shows them).
///
/// ```
/// # use space3d_platform::log_directives;
/// assert_eq!(log_directives(None, "info"), if cfg!(target_os = "ios") {
///     "info,winit::platform_impl::ios::app_state=error"
/// } else {
///     "info"
/// });
/// ```
pub fn log_directives(rust_log: Option<&str>, default: &str) -> String {
    directives(rust_log, default, cfg!(target_os = "ios"))
}

fn directives(rust_log: Option<&str>, default: &str, ios: bool) -> String {
    let base = rust_log.map(str::trim).filter(|v| !v.is_empty()).unwrap_or(default);
    if !ios || base.contains(WINIT_IOS_EVENT_ORDER_TARGET) {
        return base.to_owned();
    }
    let base = base.trim_end().trim_end_matches(',');
    let sep = if base.is_empty() { "" } else { "," };
    format!("{base}{sep}{WINIT_IOS_EVENT_ORDER_TARGET}=error")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_log_wins_and_ios_quiets_winit() {
        let default = "info,wgpu_core=warn";
        assert_eq!(directives(None, default, false), default);
        assert_eq!(directives(Some("  "), default, false), default, "blank: unset");
        assert_eq!(directives(Some("debug"), default, false), "debug");
        let quiet = "winit::platform_impl::ios::app_state=error";
        assert_eq!(directives(None, default, true), format!("{default},{quiet}"));
        assert_eq!(directives(Some("info"), default, true), format!("info,{quiet}"));
        assert_eq!(directives(Some("debug,"), default, true), format!("debug,{quiet}"));
        assert_eq!(directives(None, "", true), quiet);
        // Named: the developer's level for it stands.
        let named = "debug,winit::platform_impl::ios::app_state=info";
        assert_eq!(directives(Some(named), default, true), named);
        for d in [directives(Some("info"), default, true), directives(None, default, true)] {
            assert!(tracing_subscriber::EnvFilter::try_new(&d).is_ok(), "{d}");
        }
    }
}
