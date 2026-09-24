//! Environment switches understood by the harness.
//!
//! | variable | effect |
//! |---|---|
//! | `SPACE3D_UPDATE_SNAPSHOTS=1` / `UPDATE_SNAPSHOTS=1` | re-bless golden screenshots instead of comparing |
//! | `SPACE3D_CI=1` / `CI=true` | a missing golden is a failure instead of being written |
//! | `SPACE3D_SNAPSHOT_VARIANT=os\|backend\|os-backend` | write new/updated goldens as platform variants |
//! | `SPACE3D_REQUIRE_GPU=1` | fail (instead of skip) GPU tests when no adapter is available |
//! | `SPACE3D_NO_GPU=1` | pretend no GPU adapter exists (exercise the skip paths) |
//! | `SPACE3D_TEST_REPORT=dir` | append results to `dir/results.jsonl` and regenerate `dir/index.html` |
//! | `SPACE3D_PERF_BUDGET_SCALE=2.0` | multiply every performance budget (slow CI machines) |

/// Name of the snapshot re-bless switch.
pub const UPDATE_SNAPSHOTS: &str = "SPACE3D_UPDATE_SNAPSHOTS";
/// Name of the CI switch.
pub const CI: &str = "SPACE3D_CI";
/// Name of the golden-variant switch.
pub const SNAPSHOT_VARIANT: &str = "SPACE3D_SNAPSHOT_VARIANT";
/// Name of the strict-GPU switch.
pub const REQUIRE_GPU: &str = "SPACE3D_REQUIRE_GPU";
/// Name of the fake-no-GPU switch.
pub const NO_GPU: &str = "SPACE3D_NO_GPU";
/// Name of the report directory variable.
pub const TEST_REPORT: &str = "SPACE3D_TEST_REPORT";
/// Name of the performance budget multiplier.
pub const PERF_BUDGET_SCALE: &str = "SPACE3D_PERF_BUDGET_SCALE";

/// `true` when the variable is set to `1`, `true`, `yes` or `on` (case-insensitive).
pub fn flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| is_truthy(&v))
}

pub(crate) fn is_truthy(v: &str) -> bool {
    matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

/// Running in CI (`SPACE3D_CI` or the conventional `CI` variable).
pub fn is_ci() -> bool {
    flag(CI) || flag("CI")
}

/// Goldens should be re-blessed (`SPACE3D_UPDATE_SNAPSHOTS` or `UPDATE_SNAPSHOTS`).
pub fn update_snapshots() -> bool {
    flag(UPDATE_SNAPSHOTS) || flag("UPDATE_SNAPSHOTS")
}

/// GPU tests must not skip.
pub fn require_gpu() -> bool {
    flag(REQUIRE_GPU)
}

/// Multiplier applied to performance budgets (default 1.0).
pub fn perf_budget_scale() -> f64 {
    std::env::var(PERF_BUDGET_SCALE)
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truthy_values() {
        for v in ["1", "true", "TRUE", " yes ", "on"] {
            assert!(is_truthy(v), "{v}");
        }
        for v in ["", "0", "false", "no", "off", "2"] {
            assert!(!is_truthy(v), "{v}");
        }
    }
}
