//! GPU adapter detection and graceful skipping of GPU tests.
//!
//! GPU tests must *skip*, not fail, on machines without an adapter (headless CI). Start such
//! tests with [`require_gpu!`](crate::require_gpu) or check [`gpu_available`]:
//!
//! ```
//! fn renders_something() {
//!     space3d_test::require_gpu!();
//!     // ... GPU work ...
//! }
//! # renders_something();
//! ```
//!
//! Set `SPACE3D_REQUIRE_GPU=1` on GPU CI runners to turn skips into failures, and
//! `SPACE3D_NO_GPU=1` to exercise the skip paths on a machine that has a GPU.

use crate::env;
use std::sync::OnceLock;

/// Description of the adapter the harness renders with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GpuInfo {
    /// Adapter name, e.g. `"Apple M2"`.
    pub name: String,
    /// Backend name as used for golden variants: `metal`, `vulkan`, `dx12`, `gl`, `webgpu`...
    pub backend: String,
    /// Integrated / discrete / virtual / cpu / other.
    pub device_type: String,
    /// Driver description (may be empty).
    pub driver: String,
}

impl GpuInfo {
    /// Read the adapter info of an existing renderer.
    pub fn from_renderer(renderer: &space3d_render::Renderer) -> Self {
        let a = &renderer.capabilities().adapter;
        Self {
            name: a.name.clone(),
            backend: backend_name(a.backend),
            device_type: format!("{:?}", a.device_type).to_ascii_lowercase(),
            driver: format!("{} {}", a.driver, a.driver_info).trim().to_string(),
        }
    }
}

/// Short, file-name friendly backend name (`metal`, `vulkan`, `dx12`, `gl`, `webgpu`, `noop`).
pub fn backend_name(backend: space3d_render::wgpu::Backend) -> String {
    match backend.to_str() {
        "browserwebgpu" => "webgpu".to_string(),
        other => other.to_ascii_lowercase(),
    }
}

/// Current operating system as used for golden variants (`macos`, `linux`, `windows`, ...).
pub fn os_name() -> &'static str {
    if cfg!(target_arch = "wasm32") {
        "web"
    } else {
        std::env::consts::OS
    }
}

static PROBE: OnceLock<Result<GpuInfo, String>> = OnceLock::new();

fn probe() -> Result<GpuInfo, String> {
    if env::flag(env::NO_GPU) {
        return Err(format!("{} is set", env::NO_GPU));
    }
    #[cfg(target_arch = "wasm32")]
    {
        Err("headless rendering is not available on wasm32 (renderer creation is async)".to_string())
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut app = space3d_core::App::new();
        space3d_render::init_headless(&mut app, 4, 4).map_err(|e| e.to_string())?;
        let renderer = app.resources.expect::<space3d_render::Renderer>();
        Ok(GpuInfo::from_renderer(renderer))
    }
}

/// Probe once per process for a usable GPU adapter (creates and drops a tiny headless renderer).
pub fn gpu_info() -> Result<&'static GpuInfo, &'static str> {
    PROBE.get_or_init(probe).as_ref().map_err(String::as_str)
}

/// Whether headless rendering works in this process (cached).
pub fn gpu_available() -> bool {
    gpu_info().is_ok()
}

/// Returns `true` when a GPU test should be skipped, printing a notice. Panics instead when
/// `SPACE3D_REQUIRE_GPU=1`. Used by [`require_gpu!`](crate::require_gpu).
pub fn skip_without_gpu(test: &str) -> bool {
    match gpu_info() {
        Ok(_) => false,
        Err(reason) => {
            skip_notice(test, reason);
            true
        }
    }
}

/// Print a skip notice (or panic when `SPACE3D_REQUIRE_GPU=1`).
pub(crate) fn skip_notice(test: &str, reason: &str) {
    if env::require_gpu() {
        panic!(
            "{test}: GPU required ({} is set) but unavailable: {reason}",
            env::REQUIRE_GPU
        );
    }
    eprintln!("space3d-test: skipping {test}: no GPU adapter ({reason})");
}

/// Return early from the current test (or function) when no GPU adapter is available.
///
/// `require_gpu!()` returns `()`, `require_gpu!(value)` returns `value`.
#[macro_export]
macro_rules! require_gpu {
    () => {
        if $crate::gpu::skip_without_gpu(module_path!()) {
            return;
        }
    };
    ($ret:expr) => {
        if $crate::gpu::skip_without_gpu(module_path!()) {
            return $ret;
        }
    };
}
