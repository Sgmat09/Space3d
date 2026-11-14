//! Background mode: windows open **without stealing focus**.
//!
//! Used for automated runs (tests, smoke runs of examples, CI, AI agents) so a developer's
//! active app keeps keyboard focus while engine windows pop up behind it.
//!
//! In background mode:
//! * windows are created inactive (`with_active(false)`);
//! * on macOS the process uses the `Accessory` activation policy (no Dock icon, no menu bar
//!   takeover) and does not call `activateIgnoringOtherApps`, so it never comes to front.
//!
//! Resolution order:
//! 1. [`set_background_mode`] (programmatic override, e.g. an example's `--frames N` flag);
//! 2. `SPACE3D_BACKGROUND=1|0` (explicit on/off);
//! 3. automatic when running unattended: `SPACE3D_EXIT_AFTER_FRAMES`, `SPACE3D_EXIT_AFTER_SECS`,
//!    `CI` or `CLAUDECODE` is set.

use std::sync::atomic::{AtomicU8, Ordering};

/// Env var forcing background mode on (`1`/`true`/`yes`) or off (`0`/`false`/`no`).
pub const ENV_BACKGROUND: &str = "SPACE3D_BACKGROUND";

/// Env vars whose presence means "unattended run" and enables background mode automatically.
pub const AUTO_BACKGROUND_ENVS: &[&str] = &[
    "SPACE3D_EXIT_AFTER_FRAMES",
    "SPACE3D_EXIT_AFTER_SECS",
    "CI",
    "CLAUDECODE",
];

const UNSET: u8 = 0;
const OFF: u8 = 1;
const ON: u8 = 2;
static OVERRIDE: AtomicU8 = AtomicU8::new(UNSET);

/// Force background mode on/off for this process (takes precedence over env vars).
/// Call before the runner starts (e.g. at the top of `main`).
pub fn set_background_mode(on: bool) {
    OVERRIDE.store(if on { ON } else { OFF }, Ordering::Relaxed);
}

/// Whether windows should open without taking focus.
pub fn background_mode() -> bool {
    match OVERRIDE.load(Ordering::Relaxed) {
        ON => true,
        OFF => false,
        // The environment is read once: this is called every frame by the runner.
        _ => *ENV_MODE.get_or_init(|| resolve(|k| std::env::var(k).ok())),
    }
}

static ENV_MODE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

fn resolve(get: impl Fn(&str) -> Option<String>) -> bool {
    if let Some(v) = get(ENV_BACKGROUND) {
        match v.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => return true,
            "0" | "false" | "no" | "off" => return false,
            _ => {}
        }
    }
    AUTO_BACKGROUND_ENVS
        .iter()
        .any(|k| get(k).is_some_and(|v| !v.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn explicit_env_wins_over_auto_detection() {
        assert!(resolve(env(&[("SPACE3D_BACKGROUND", "1")])));
        assert!(!resolve(env(&[("SPACE3D_BACKGROUND", "0"), ("CI", "true")])));
    }

    #[test]
    fn unattended_runs_are_background_by_default() {
        assert!(resolve(env(&[("CI", "true")])));
        assert!(resolve(env(&[("CLAUDECODE", "1")])));
        assert!(resolve(env(&[("SPACE3D_EXIT_AFTER_FRAMES", "120")])));
        assert!(!resolve(env(&[])));
        assert!(!resolve(env(&[("CI", "")])));
    }
}
