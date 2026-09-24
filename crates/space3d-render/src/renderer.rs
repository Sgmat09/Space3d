//! [`Renderer`]: device/queue ownership, surfaces, render targets, asset upload and capture.

use crate::assets::RenderAssets;
use crate::frame::FrameState;
use crate::gpu::{Uploader, align_up};
use crate::pipeline::{DEPTH_FORMAT, OFFSCREEN_FORMAT, Pipelines};
use crate::stats::{DrawPath, RenderStats};
use crate::texture::{SamplerCache, create_gpu_texture, format_supported, white_texture};
use space3d_core::ImageData;
use space3d_tasks::Task;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Renderer errors.
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    /// No suitable GPU adapter (headless CI without GPU, unsupported browser...).
    #[error("no suitable GPU adapter: {0}")]
    NoAdapter(String),
    /// Device creation failed.
    #[error("failed to create device: {0}")]
    Device(String),
    /// Surface creation/configuration failed.
    #[error("surface error: {0}")]
    Surface(String),
    /// GPU readback failed.
    #[error("capture failed: {0}")]
    Capture(String),
}

/// Construction options.
#[derive(Clone, Debug)]
pub struct RendererConfig {
    /// Initial width of the primary target (window surface or offscreen).
    pub width: u32,
    /// Initial height of the primary target.
    pub height: u32,
    /// MSAA samples for the primary target (1 or 4; clamped to what the device supports).
    pub msaa_samples: u32,
    /// Wait for vblank when presenting.
    pub vsync: bool,
    /// Adapter preference.
    pub power_preference: wgpu::PowerPreference,
    /// Restrict backends (`None` = platform default, overridable with `WGPU_BACKEND`).
    pub backends: Option<wgpu::Backends>,
    /// Use a software adapter (e.g. lavapipe/WARP) if available.
    pub force_fallback_adapter: bool,
}

impl Default for RendererConfig {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            msaa_samples: 1,
            vsync: true,
            power_preference: wgpu::PowerPreference::HighPerformance,
            backends: None,
            force_fallback_adapter: false,
        }
    }
}

/// What the adapter can do and which draw path was selected.
#[derive(Clone, Debug)]
pub struct RenderCapabilities {
    /// Adapter description.
    pub adapter: wgpu::AdapterInfo,
    /// Compute shaders + enough storage buffers for GPU culling.
    pub compute: bool,
    /// Indirect draws supported.
    pub indirect: bool,
    /// `first_instance` honoured by indirect draws.
    pub indirect_first_instance: bool,
    /// `multi_draw_indexed_indirect_count` available.
    pub multi_draw_indirect_count: bool,
    /// Non-zero `base_vertex` in direct indexed draws (`DownlevelFlags::BASE_VERTEX`). Missing
    /// on WebGL2 and GLES < 3.2, where wgpu does not validate it and the GL backend calls an
    /// entry point that does not exist (a panic on the web). Without it, direct draws rebind
    /// the arena's vertex buffer at each mesh's first vertex and draw with `base_vertex = 0`,
    /// and GPU-driven paths are not offered.
    pub base_vertex: bool,
    /// BC / ASTC / ETC2 compressed textures.
    pub texture_compression: (bool, bool, bool),
    /// Best draw path for this device.
    pub best_draw_path: DrawPath,
    /// MSAA sample counts usable on the primary target.
    pub max_msaa: u32,
    /// Largest storage binding (bounds GPU-culled buffers).
    pub max_storage_binding: u64,
    /// Largest buffer the device can create (bounds the mesh arena and instance buffers).
    pub max_buffer_size: u64,
}

impl RenderCapabilities {
    fn detect(adapter: &wgpu::Adapter, device: &wgpu::Device) -> Self {
        let dl = adapter.get_downlevel_capabilities();
        let limits = device.limits();
        let features = device.features();
        let compute = dl.flags.contains(wgpu::DownlevelFlags::COMPUTE_SHADERS)
            && limits.max_storage_buffers_per_shader_stage >= 4
            && limits.max_compute_workgroup_size_x >= 64
            && limits.max_compute_invocations_per_workgroup >= 64;
        let indirect = dl.flags.contains(wgpu::DownlevelFlags::INDIRECT_EXECUTION);
        let base_vertex = dl.flags.contains(wgpu::DownlevelFlags::BASE_VERTEX);
        let ifi = features.contains(wgpu::Features::INDIRECT_FIRST_INSTANCE);
        let mdic = features.contains(wgpu::Features::MULTI_DRAW_INDIRECT_COUNT);
        let best_draw_path = best_draw_path(compute && indirect, base_vertex, ifi, mdic);
        let msaa4 = adapter
            .get_texture_format_features(OFFSCREEN_FORMAT)
            .flags
            .sample_count_supported(4)
            && adapter
                .get_texture_format_features(DEPTH_FORMAT)
                .flags
                .sample_count_supported(4);
        Self {
            adapter: adapter.get_info(),
            compute,
            indirect,
            indirect_first_instance: ifi,
            multi_draw_indirect_count: mdic,
            base_vertex,
            texture_compression: (
                features.contains(wgpu::Features::TEXTURE_COMPRESSION_BC),
                features.contains(wgpu::Features::TEXTURE_COMPRESSION_ASTC),
                features.contains(wgpu::Features::TEXTURE_COMPRESSION_ETC2),
            ),
            best_draw_path,
            max_msaa: if msaa4 { 4 } else { 1 },
            // Storage bindings must be a multiple of 4 bytes and never exceed the buffer.
            max_storage_binding: limits.max_storage_buffer_binding_size.min(limits.max_buffer_size) & !3,
            max_buffer_size: limits.max_buffer_size,
        }
    }
}

/// Best draw path for a device. GPU-driven paths need compute + indirect draws, and are only
/// offered with `BASE_VERTEX`: devices without it (WebGL2, GLES 3.0/3.1) are the downlevel
/// targets `CpuDirect` exists for, and it is the path whose direct draws emulate `base_vertex`.
fn best_draw_path(compute_indirect: bool, base_vertex: bool, first_instance: bool, multi_count: bool) -> DrawPath {
    match (compute_indirect && base_vertex, first_instance, multi_count) {
        (true, true, true) => DrawPath::GpuMultiDrawCount,
        (true, true, false) => DrawPath::GpuMultiDraw,
        (true, false, _) => DrawPath::GpuIndirect,
        _ => DrawPath::CpuDirect,
    }
}

/// Color/MSAA/depth attachments for one render target size.
pub(crate) struct Attachments {
    pub width: u32,
    pub height: u32,
    pub samples: u32,
    pub format: wgpu::TextureFormat,
    /// Single-sample color texture (offscreen only; surfaces provide their own).
    pub color: Option<wgpu::Texture>,
    pub color_view: Option<wgpu::TextureView>,
    pub msaa_view: Option<wgpu::TextureView>,
    pub depth_view: wgpu::TextureView,
}

impl Attachments {
    pub fn new(
        device: &wgpu::Device,
        width: u32,
        height: u32,
        samples: u32,
        format: wgpu::TextureFormat,
        offscreen: bool,
    ) -> Self {
        let size = wgpu::Extent3d {
            width: width.max(1),
            height: height.max(1),
            depth_or_array_layers: 1,
        };
        let tex = |label, format, samples, usage| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size,
                mip_level_count: 1,
                sample_count: samples,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: &[],
            })
        };
        let color = offscreen.then(|| {
            tex(
                "space3d offscreen color",
                format,
                1,
                wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::COPY_SRC
                    | wgpu::TextureUsages::TEXTURE_BINDING,
            )
        });
        let msaa_view = (samples > 1).then(|| {
            tex(
                "space3d msaa color",
                format,
                samples,
                wgpu::TextureUsages::RENDER_ATTACHMENT,
            )
            .create_view(&Default::default())
        });
        let depth = tex(
            "space3d depth",
            DEPTH_FORMAT,
            samples,
            wgpu::TextureUsages::RENDER_ATTACHMENT,
        );
        Self {
            width: size.width,
            height: size.height,
            samples,
            format,
            color_view: color.as_ref().map(|c| c.create_view(&Default::default())),
            color,
            msaa_view,
            depth_view: depth.create_view(&Default::default()),
        }
    }
}

pub(crate) struct SurfaceState {
    pub surface: wgpu::Surface<'static>,
    pub config: wgpu::SurfaceConfiguration,
    /// Format of the views frames render into: `config.format`, or its sRGB twin (listed in
    /// `config.view_formats`) when the surface itself has no sRGB format.
    pub view_format: wgpu::TextureFormat,
    /// The shader encodes sRGB itself (no sRGB format or view available).
    pub manual_srgb: bool,
    pub attachments: Attachments,
    /// A frame was acquired with the current configuration.
    pub presented: bool,
}

impl SurfaceState {
    /// Render without the sRGB view format: reconfigure the surface without it and encode sRGB
    /// in the shader instead.
    pub(crate) fn drop_srgb_view(&mut self, device: &wgpu::Device) {
        self.config.view_formats.clear();
        self.view_format = self.config.format;
        self.manual_srgb = !self.view_format.is_srgb();
        let a = &self.attachments;
        self.attachments = Attachments::new(device, a.width, a.height, a.samples, self.view_format, false);
        self.surface.configure(device, &self.config);
    }
}

/// Whether a surface that reported `Lost` should be reconfigured without its sRGB view format
/// instead of being given up. On the web, wgpu reports a canvas configuration the browser
/// rejected as `Lost` (native wgpu validates view formats when configuring instead), so a
/// canvas that never produced a frame with a view format may have been refused because of it.
/// Recreating the surface would configure it the same way again, forever.
pub(crate) fn retry_without_view_formats(web: bool, presented: bool, config: &wgpu::SurfaceConfiguration) -> bool {
    web && !presented && !config.view_formats.is_empty()
}

/// Surface texture format and the format frames are rendered in, from a surface's supported
/// formats (`None` if there are none). An sRGB format is preferred. Browser WebGPU canvases
/// offer only `Bgra8Unorm`/`Rgba8Unorm`(/`Rgba16Float`), but allow an sRGB *view* of the
/// canvas texture: rendering through it keeps blending and MSAA resolves in linear space,
/// like native sRGB surfaces. Only formats without an sRGB twin (or devices without
/// `SURFACE_VIEW_FORMATS`) fall back to encoding sRGB in the shader, where blending then
/// operates on encoded values.
pub(crate) fn surface_formats(
    formats: &[wgpu::TextureFormat],
    view_formats_supported: bool,
) -> Option<(wgpu::TextureFormat, wgpu::TextureFormat)> {
    let first = *formats.first()?;
    if let Some(srgb) = formats.iter().copied().find(|f| f.is_srgb()) {
        return Some((srgb, srgb));
    }
    let view = first.add_srgb_suffix();
    Some((first, if view_formats_supported { view } else { first }))
}

/// The GPU renderer. Insert it as a resource (the platform runner does this for windows; use
/// [`Renderer::new_headless`] for tests/tools). Rendering happens in `Stage::Render`.
pub struct Renderer {
    pub(crate) instance: wgpu::Instance,
    pub(crate) adapter: wgpu::Adapter,
    pub(crate) device: wgpu::Device,
    pub(crate) queue: wgpu::Queue,
    pub(crate) caps: RenderCapabilities,
    pub(crate) config: RendererConfig,
    pub(crate) surface: Option<SurfaceState>,
    /// Capture target; also the primary target when headless.
    pub(crate) offscreen: Attachments,
    pub(crate) pipelines: Pipelines,
    pub(crate) samplers: SamplerCache,
    pub(crate) white: wgpu::TextureView,
    pub(crate) white_sampler: wgpu::Sampler,
    pub(crate) uploader: Uploader,
    pub(crate) frame: FrameState,
    pub(crate) stats: RenderStats,
    pub(crate) frame_index: u64,
    /// Uncaptured wgpu errors so far (see [`install_error_handler`]).
    pub(crate) gpu_errors: Arc<AtomicU64>,
}

impl std::fmt::Debug for Renderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Renderer")
            .field("adapter", &self.caps.adapter.name)
            .field("backend", &self.caps.adapter.backend)
            .field("size", &self.size())
            .field("draw_path", &self.caps.best_draw_path)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
thread_local! {
    /// `(max_buffer_size, max_storage_buffer_binding_size)` requested instead of the adapter's
    /// limits (when lower) by renderers created on this thread, so tests run against a device that really
    /// enforces smaller limits (wgpu validation fails past them) — see
    /// `frame::tests::limited_app`.
    pub(crate) static TEST_LIMITS: std::cell::Cell<Option<(u64, u64)>> = const { std::cell::Cell::new(None) };
}

/// Route wgpu errors that no error scope captured through `tracing` instead of wgpu's default
/// handler, which panics. With `panic` (debug builds and tests) the error still panics after
/// being logged so bugs surface immediately; release builds log and keep running — skipping a
/// broken command buffer is better than crashing the game. Errors tend to repeat every frame,
/// so only the first ones and then every 1000th are logged.
pub(crate) fn install_error_handler(device: &wgpu::Device, panic: bool) -> Arc<AtomicU64> {
    let count = Arc::new(AtomicU64::new(0));
    let counter = count.clone();
    device.on_uncaptured_error(Arc::new(move |err: wgpu::Error| {
        let n = counter.fetch_add(1, Ordering::Relaxed) + 1;
        if n <= 16 || n.is_multiple_of(1000) {
            tracing::error!(occurrences = n, "wgpu error: {err}");
        }
        if panic {
            panic!("wgpu error: {err}");
        }
    }));
    device.set_device_lost_callback(|reason, message| match reason {
        wgpu::DeviceLostReason::Destroyed => tracing::debug!(%message, "GPU device destroyed"),
        _ => tracing::error!(?reason, %message, "GPU device lost"),
    });
    count
}

/// Display connection handed to wgpu (needed by GLES on Wayland/X11; unused elsewhere).
pub type DisplayHandle = Box<dyn wgpu::wgt::WgpuHasDisplayHandle>;

async fn create_instance(backends: Option<wgpu::Backends>, display: Option<DisplayHandle>) -> wgpu::Instance {
    let mut desc = match display {
        Some(d) => wgpu::InstanceDescriptor::new_with_display_handle_from_env(d),
        None => wgpu::InstanceDescriptor::new_without_display_handle_from_env(),
    };
    if let Some(b) = backends {
        desc.backends = b;
    }
    #[cfg(target_arch = "wasm32")]
    {
        wgpu::util::new_instance_with_webgpu_detection(desc).await
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        wgpu::Instance::new(desc)
    }
}

impl Renderer {
    /// Headless renderer with an offscreen `width`x`height` target (default settings).
    pub async fn new_headless(width: u32, height: u32) -> Result<Self, RenderError> {
        Self::new_headless_with(RendererConfig {
            width,
            height,
            ..Default::default()
        })
        .await
    }

    /// Headless renderer with explicit configuration.
    pub async fn new_headless_with(config: RendererConfig) -> Result<Self, RenderError> {
        let instance = create_instance(config.backends, None).await;
        Self::from_instance(instance, None, config).await
    }

    /// Renderer presenting to a window/canvas. `target` is typically an `Arc<winit::Window>`
    /// (native) or a `wgpu::SurfaceTarget::Canvas` (web).
    pub async fn new_with_surface(
        target: impl Into<wgpu::SurfaceTarget<'static>>,
        config: RendererConfig,
    ) -> Result<Self, RenderError> {
        Self::new_with_surface_and_display(target, None, config).await
    }

    /// Like [`Self::new_with_surface`], also passing the platform display connection (e.g.
    /// winit's `OwnedDisplayHandle`), which the GLES backend needs on Wayland/X11.
    pub async fn new_with_surface_and_display(
        target: impl Into<wgpu::SurfaceTarget<'static>>,
        display: Option<DisplayHandle>,
        config: RendererConfig,
    ) -> Result<Self, RenderError> {
        let instance = create_instance(config.backends, display).await;
        let surface = instance
            .create_surface(target)
            .map_err(|e| RenderError::Surface(e.to_string()))?;
        Self::from_instance(instance, Some(surface), config).await
    }

    async fn from_instance(
        instance: wgpu::Instance,
        surface: Option<wgpu::Surface<'static>>,
        mut config: RendererConfig,
    ) -> Result<Self, RenderError> {
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: config.power_preference,
                force_fallback_adapter: config.force_fallback_adapter,
                compatible_surface: surface.as_ref(),
                apply_limit_buckets: false,
            })
            .await
            .map_err(|e| RenderError::NoAdapter(e.to_string()))?;
        let wanted = wgpu::Features::INDIRECT_FIRST_INSTANCE
            | wgpu::Features::MULTI_DRAW_INDIRECT_COUNT
            | wgpu::Features::FLOAT32_FILTERABLE
            | wgpu::Features::TEXTURE_COMPRESSION_BC
            | wgpu::Features::TEXTURE_COMPRESSION_ASTC
            | wgpu::Features::TEXTURE_COMPRESSION_ETC2;
        #[cfg_attr(not(test), expect(unused_mut))]
        let mut required_limits = adapter.limits();
        #[cfg(test)]
        if let Some((buffer, binding)) = TEST_LIMITS.get() {
            required_limits.max_buffer_size = buffer.min(required_limits.max_buffer_size);
            required_limits.max_storage_buffer_binding_size =
                binding.min(required_limits.max_storage_buffer_binding_size);
        }
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("space3d device"),
                required_features: adapter.features() & wanted,
                required_limits,
                ..Default::default()
            })
            .await
            .map_err(|e| RenderError::Device(e.to_string()))?;
        let gpu_errors = install_error_handler(&device, cfg!(debug_assertions));
        let caps = RenderCapabilities::detect(&adapter, &device);
        tracing::info!(
            adapter = %caps.adapter.name,
            backend = ?caps.adapter.backend,
            draw_path = ?caps.best_draw_path,
            "space3d renderer initialised"
        );
        config.msaa_samples = if config.msaa_samples > 1 { caps.max_msaa } else { 1 };
        config.width = config.width.max(1);
        config.height = config.height.max(1);
        let offscreen = Attachments::new(
            &device,
            config.width,
            config.height,
            config.msaa_samples,
            OFFSCREEN_FORMAT,
            true,
        );
        let pipelines = Pipelines::new(&device, caps.compute && caps.indirect);
        let white = white_texture(&device, &queue);
        let mut samplers = SamplerCache::default();
        let white_sampler = samplers.get(&device, Default::default());
        let frame = FrameState::new(&caps, &device);
        let uploader = Uploader::new(&device);
        let mut renderer = Self {
            instance,
            adapter,
            device,
            queue,
            caps,
            config,
            surface: None,
            offscreen,
            pipelines,
            samplers,
            white,
            white_sampler,
            uploader,
            frame,
            stats: RenderStats::default(),
            frame_index: 0,
            gpu_errors,
        };
        if let Some(surface) = surface {
            renderer.configure_surface(surface)?;
        }
        Ok(renderer)
    }

    fn configure_surface(&mut self, surface: wgpu::Surface<'static>) -> Result<(), RenderError> {
        let caps = surface.get_capabilities(&self.adapter);
        let view_formats_supported = self
            .adapter
            .get_downlevel_capabilities()
            .flags
            .contains(wgpu::DownlevelFlags::SURFACE_VIEW_FORMATS);
        let Some((format, view_format)) = surface_formats(&caps.formats, view_formats_supported) else {
            return Err(RenderError::Surface("surface not supported by adapter".into()));
        };
        let present_mode = if self.config.vsync {
            wgpu::PresentMode::AutoVsync
        } else {
            wgpu::PresentMode::AutoNoVsync
        };
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: self.config.width,
            height: self.config.height,
            present_mode,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps
                .alpha_modes
                .first()
                .copied()
                .unwrap_or(wgpu::CompositeAlphaMode::Auto),
            view_formats: if view_format != format {
                vec![view_format]
            } else {
                vec![]
            },
        };
        surface.configure(&self.device, &config);
        let samples = if self.config.msaa_samples > 1
            && self
                .adapter
                .get_texture_format_features(view_format)
                .flags
                .sample_count_supported(self.config.msaa_samples)
        {
            self.config.msaa_samples
        } else {
            1
        };
        // The MSAA and depth attachments are separate textures: they use the view format.
        let attachments = Attachments::new(&self.device, config.width, config.height, samples, view_format, false);
        self.surface = Some(SurfaceState {
            surface,
            view_format,
            manual_srgb: !view_format.is_srgb(),
            config,
            attachments,
            presented: false,
        });
        Ok(())
    }

    /// Attach (or replace) the presentation surface — e.g. on Android/iOS resume.
    pub fn set_surface(&mut self, target: impl Into<wgpu::SurfaceTarget<'static>>) -> Result<(), RenderError> {
        let surface = self
            .instance
            .create_surface(target)
            .map_err(|e| RenderError::Surface(e.to_string()))?;
        if !self.adapter.is_surface_supported(&surface) {
            return Err(RenderError::Surface("adapter cannot present to this surface".into()));
        }
        self.configure_surface(surface)
    }

    /// Drop the presentation surface (Android suspend). Rendering continues offscreen.
    pub fn remove_surface(&mut self) {
        self.surface = None;
    }

    /// Whether a presentation surface is attached.
    pub fn has_surface(&self) -> bool {
        self.surface.is_some()
    }

    /// Resize the primary target. The window surface is reconfigured now; the headless
    /// offscreen target is reallocated by the next frame, so [`Self::capture`] until then
    /// still returns the last rendered frame.
    pub fn resize(&mut self, width: u32, height: u32) {
        let (width, height) = (width.max(1), height.max(1));
        self.config.width = width;
        self.config.height = height;
        if let Some(s) = &mut self.surface {
            s.config.width = width;
            s.config.height = height;
            s.surface.configure(&self.device, &s.config);
            s.attachments = Attachments::new(&self.device, width, height, s.attachments.samples, s.view_format, false);
        }
    }

    /// Headless: (re)allocate the offscreen target to the configured size and sample count.
    /// Called at the start of a frame, never between a frame and its capture.
    pub(crate) fn ensure_offscreen(&mut self) {
        if self.surface.is_some() {
            // Windowed: the offscreen target only serves captures, which size it themselves.
            return;
        }
        let (w, h, samples) = (self.config.width, self.config.height, self.config.msaa_samples);
        let a = &self.offscreen;
        if (a.width, a.height, a.samples) != (w, h, samples) {
            self.offscreen = Attachments::new(&self.device, w, h, samples, OFFSCREEN_FORMAT, true);
        }
    }

    /// Change MSAA sample count (1 or 4; clamped to device support). Surface attachments are
    /// recreated now, the headless offscreen target on the next frame.
    pub fn set_msaa_samples(&mut self, samples: u32) {
        let samples = if samples > 1 { self.caps.max_msaa } else { 1 };
        if samples == self.config.msaa_samples {
            return;
        }
        self.config.msaa_samples = samples;
        if let Some(s) = &mut self.surface {
            let ok = self
                .adapter
                .get_texture_format_features(s.view_format)
                .flags
                .sample_count_supported(samples);
            let samples = if ok { samples } else { 1 };
            s.attachments = Attachments::new(
                &self.device,
                s.config.width,
                s.config.height,
                samples,
                s.view_format,
                false,
            );
        }
    }

    /// MSAA sample count of the primary target.
    pub fn msaa_samples(&self) -> u32 {
        self.config.msaa_samples
    }

    /// Current primary target size.
    pub fn size(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    /// The wgpu device.
    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    /// The wgpu queue.
    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    /// Adapter capabilities and chosen draw path.
    pub fn capabilities(&self) -> &RenderCapabilities {
        &self.caps
    }

    /// Stats of the last rendered frame.
    pub fn stats(&self) -> &RenderStats {
        &self.stats
    }

    /// Format frames are rendered in when windowed (for overlays drawing into the same view):
    /// the surface format, or its sRGB view format where the surface itself has no sRGB format
    /// (browser WebGPU canvases). It changes if a browser rejects the sRGB view format, so
    /// overlays should check it every frame.
    pub fn surface_format(&self) -> Option<wgpu::TextureFormat> {
        self.surface.as_ref().map(|s| s.view_format)
    }

    /// Whether textures of `format` can be uploaded and sampled by materials: compressed
    /// formats need device features, and the format must be filterable float (integer and
    /// depth formats never are; 32-bit float only with `FLOAT32_FILTERABLE`).
    pub fn supports_texture_format(&self, format: wgpu::TextureFormat) -> bool {
        format_supported(&self.adapter, &self.device, format)
    }

    /// Build error of the current version of a [`crate::CustomShader`], once a draw has used it
    /// (its draws then use the built-in shader). `None` while it builds fine or is unused.
    pub fn shader_error(&self, shader: crate::ShaderHandle) -> Option<&str> {
        self.pipelines.custom_error(shader)
    }

    /// Number of wgpu errors that reached the uncaptured-error handler so far. Release builds
    /// log these instead of panicking; tests and tools can assert that it stays 0.
    pub fn gpu_error_count(&self) -> u64 {
        self.gpu_errors.load(Ordering::Relaxed)
    }

    /// Upload all pending meshes/textures now (normally done at the start of each frame).
    pub fn upload_assets(&mut self, assets: &mut RenderAssets) {
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("space3d asset upload"),
        });
        self.upload_assets_into(&mut encoder, assets);
        self.uploader.finish();
        self.queue.submit([encoder.finish()]);
        self.uploader.recall();
    }

    /// Encode pending asset uploads (mesh arena + textures).
    pub(crate) fn upload_assets_into(&mut self, encoder: &mut wgpu::CommandEncoder, assets: &mut RenderAssets) {
        // Free removed meshes first so their space can be reused this frame.
        for a in assets.freed_allocs.drain(..) {
            assets.arena.free(a);
        }
        if !assets.pending_meshes.is_empty() {
            self.upload_meshes(encoder, assets);
        }
        if !assets.pending_textures.is_empty() {
            let pending = std::mem::take(&mut assets.pending_textures);
            for h in &pending {
                let Some(entry) = assets.textures.get_mut(h.raw()) else {
                    continue;
                };
                if entry.gpu.is_some() && entry.source.is_none() && !entry.render_target {
                    continue;
                }
                if entry.render_target && entry.gpu.is_some() {
                    continue;
                }
                match create_gpu_texture(
                    &self.adapter,
                    &self.device,
                    &self.queue,
                    encoder,
                    &mut self.pipelines,
                    &mut self.samplers,
                    entry,
                ) {
                    Ok(bytes) => {
                        self.uploader.bytes += bytes;
                        self.stats.textures_uploaded += 1;
                    }
                    Err(e) => tracing::error!(texture = ?h, "texture upload failed: {e}"),
                }
            }
            assets.pending_textures = pending;
            assets.pending_textures.clear();
        }
    }

    fn upload_meshes(&mut self, encoder: &mut wgpu::CommandEncoder, assets: &mut RenderAssets) {
        let pending = std::mem::take(&mut assets.pending_meshes);
        // Convert; keep the existing placement when the size is unchanged.
        let scratch = &mut self.frame.mesh_scratch;
        scratch.clear();
        for &h in &pending {
            let Some(entry) = assets.meshes.get_mut(h.raw()) else {
                continue;
            };
            let Some(data) = entry.cpu.take() else {
                continue;
            };
            let mut v = Vec::new();
            let mut i = Vec::new();
            crate::arena::build_vertices(&data, &mut v, &mut i);
            let (vc, ic) = (v.len() as u32, i.len() as u32);
            let alloc = match entry.gpu {
                Some(g) if g.alloc.vertex_count == vc && g.alloc.index_count == ic => Some(g.alloc),
                Some(g) => {
                    assets.arena.free(g.alloc);
                    entry.gpu = None;
                    None
                }
                None => None,
            };
            scratch.push((h, alloc, v, i, data));
        }

        // Allocate, growing only the buffer(s) that are short, until everything fits or the
        // device's max_buffer_size is reached.
        let arena = &mut assets.arena;
        let old = (arena.vertices.capacity(), arena.indices.capacity());
        if arena.vertex.is_none() || arena.index.is_none() {
            arena.grow_capacity(0, 0, self.caps.max_buffer_size);
        }
        // A mesh larger than a whole buffer can ever be is not worth growing for.
        let (max_v, max_i) = crate::arena::MeshArena::max_capacity(self.caps.max_buffer_size);
        let fits = |v: &Vec<_>, i: &Vec<u32>| v.len() as u64 <= max_v as u64 && i.len() as u64 <= max_i as u64;
        loop {
            let (mut short_v, mut short_i) = (0u32, 0u32);
            for (_, alloc, v, i, _) in scratch.iter_mut().filter(|e| e.1.is_none() && fits(&e.2, &e.3)) {
                match arena.try_alloc(v.len() as u32, i.len() as u32) {
                    Ok(a) => *alloc = Some(a),
                    Err(short) => {
                        if short.vertices {
                            short_v = short_v.saturating_add(v.len() as u32);
                        }
                        if short.indices {
                            short_i = short_i.saturating_add(i.len() as u32);
                        }
                    }
                }
            }
            if (short_v, short_i) == (0, 0) {
                break;
            }
            let before = (arena.vertices.capacity(), arena.indices.capacity());
            arena.grow_capacity(short_v, short_i, self.caps.max_buffer_size);
            if (arena.vertices.capacity(), arena.indices.capacity()) == before {
                break; // At the device limit: what is left does not fit.
            }
        }
        if arena.vertex.is_none()
            || arena.index.is_none()
            || (arena.vertices.capacity(), arena.indices.capacity()) != old
        {
            // Recorded before the writes below, so they land after the old contents' copy.
            arena.realloc_gpu(&self.device, encoder, old, &mut self.uploader);
        }
        arena.write_all(
            encoder,
            scratch,
            |(_, alloc, v, i, _)| {
                alloc.map(|alloc| crate::arena::ArenaWrite {
                    alloc,
                    vertices: v,
                    indices: i,
                })
            },
            &mut self.uploader,
        );
        for (h, alloc, v, i, data) in scratch.drain(..) {
            let Some(entry) = assets.meshes.get_mut(h.raw()) else {
                continue;
            };
            let Some(alloc) = alloc else {
                tracing::error!(
                    mesh = ?h,
                    vertices = v.len(),
                    indices = i.len(),
                    max_buffer_size = self.caps.max_buffer_size,
                    "mesh does not fit the mesh arena; it is not drawn (update or remove it)"
                );
                entry.cpu = Some(data);
                continue;
            };
            entry.gpu = Some(crate::assets::GpuMesh {
                alloc,
                sphere: entry.sphere,
            });
            self.stats.meshes_uploaded += 1;
        }
    }

    /// Read back the primary target of the last rendered frame as sRGB RGBA8.
    ///
    /// Headless: copies the offscreen target. Windowed: re-renders the last frame's primary
    /// views into the offscreen target first (surface textures are not readable).
    /// Poll the returned task from a system, `.block()` it in tests, or `.wait().await` it.
    ///
    /// Never blocks: on native a background task resolves it with non-blocking device polls
    /// (each frame's own poll resolves it too). A blocking `device.poll` would hold wgpu's
    /// device-wide snatch lock for the whole GPU wait, stalling `queue.present` and buffer
    /// destruction on the main thread until the GPU caught up.
    pub fn capture(&mut self) -> Task<Result<ImageData, RenderError>> {
        #[cfg_attr(target_arch = "wasm32", allow(unused_mut))]
        let (readback, mut rx, _) = self.submit_capture();
        #[cfg(not(target_arch = "wasm32"))]
        {
            let device = self.device.clone();
            space3d_tasks::spawn(async move {
                let mapped = loop {
                    let _ = device.poll(wgpu::PollType::Poll);
                    match rx.try_recv() {
                        Ok(Some(r)) => break Ok(r),
                        Ok(None) => tokio::time::sleep(std::time::Duration::from_millis(1)).await,
                        Err(_) => break Err(()),
                    }
                };
                readback.read(mapped)
            })
        }
        #[cfg(target_arch = "wasm32")]
        {
            // The browser (WebGPU) or the per-frame device poll (WebGL) resolves the mapping.
            space3d_tasks::spawn(async move { readback.read(rx.await.map_err(|_| ())) })
        }
    }

    /// Record and submit the copy of the offscreen target into a `MAP_READ` buffer; returns
    /// the readback, the mapping result channel and the submission.
    fn submit_capture(&mut self) -> (CaptureReadback, MapReceiver, wgpu::SubmissionIndex) {
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("space3d capture"),
        });
        if self.surface.is_some() {
            self.encode_primary_to_offscreen(&mut encoder);
        }
        let (w, h) = (self.offscreen.width, self.offscreen.height);
        let padded = align_up(w as u64 * 4, wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as u64);
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("space3d capture readback"),
            size: padded * h as u64,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let color = self.offscreen.color.as_ref().expect("offscreen color");
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: color,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded as u32),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        let (tx, rx) = futures::channel::oneshot::channel();
        encoder.map_buffer_on_submit(&buffer, wgpu::MapMode::Read, .., move |r| {
            let _ = tx.send(r);
        });
        let submission = self.queue.submit([encoder.finish()]);
        let readback = CaptureReadback {
            buffer,
            size: (w, h),
            padded,
        };
        (readback, rx, submission)
    }

    /// [`Self::capture`] and wait for the result on the calling thread (tests/tools only;
    /// never from a system).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn capture_blocking(&mut self) -> Result<ImageData, RenderError> {
        let (readback, mut rx, submission) = self.submit_capture();
        let mapped = loop {
            match rx.try_recv() {
                Ok(Some(r)) => break Ok(r),
                Ok(None) => {}
                Err(_) => break Err(()),
            }
            let waited = self.device.poll(wgpu::PollType::Wait {
                submission_index: Some(submission.clone()),
                timeout: None,
            });
            if waited.is_err() {
                // Device lost: the callback has fired with an error by now, or never will.
                break rx.try_recv().ok().flatten().ok_or(());
            }
        };
        readback.read(mapped)
    }

    /// Non-blocking device poll: fires pending map callbacks (captures, staging recycling).
    pub fn poll(&self) {
        let _ = self.device.poll(wgpu::PollType::Poll);
    }
}

/// Receives the result of a capture buffer's mapping.
type MapReceiver = futures::channel::oneshot::Receiver<Result<(), wgpu::BufferAsyncError>>;

/// A submitted capture copy: `MAP_READ` buffer with rows padded to `padded` bytes.
struct CaptureReadback {
    buffer: wgpu::Buffer,
    size: (u32, u32),
    padded: u64,
}

impl CaptureReadback {
    /// Strip the row padding of the mapped buffer (`Err(())`: the callback was dropped).
    fn read(self, mapped: Result<Result<(), wgpu::BufferAsyncError>, ()>) -> Result<ImageData, RenderError> {
        mapped
            .map_err(|()| RenderError::Capture("map callback dropped".into()))?
            .map_err(|e| RenderError::Capture(e.to_string()))?;
        let (w, h) = self.size;
        let view = self
            .buffer
            .slice(..)
            .get_mapped_range()
            .map_err(|e| RenderError::Capture(format!("{e:?}")))?;
        let row = w as usize * 4;
        let mut pixels = Vec::with_capacity(row * h as usize);
        for y in 0..h as usize {
            let start = y * self.padded as usize;
            pixels.extend_from_slice(&view[start..start + row]);
        }
        drop(view);
        self.buffer.unmap();
        Ok(ImageData {
            width: w,
            height: h,
            pixels,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use space3d_core::MeshData;

    fn renderer() -> Option<Renderer> {
        match pollster::block_on(Renderer::new_headless(16, 16)) {
            Ok(r) => Some(r),
            Err(e) => {
                eprintln!("skipping GPU test: no adapter available ({e})");
                None
            }
        }
    }

    /// Regression: arena growth doubled vertex and index buffers in lockstep and never looked
    /// at `max_buffer_size`, so create_buffer failed validation (a panic) long before the data
    /// reached the limit. Growth is now per buffer, clamped, and meshes that cannot fit are
    /// reported and kept on the CPU instead.
    #[test]
    fn mesh_arena_respects_max_buffer_size() {
        let Some(mut r) = renderer() else { return };
        r.caps.max_buffer_size = 36 * 16_384; // 16384 vertices
        let mut assets = RenderAssets::default();
        let small: Vec<_> = (0..10).map(|_| assets.add_mesh(MeshData::cube(1.0))).collect();
        let big = assets.add_mesh(MeshData::sphere(1.0, 200, 100)); // 20301 vertices
        r.upload_assets(&mut assets);
        assert!(small.iter().all(|m| assets.is_mesh_uploaded(*m)));
        assert!(!assets.is_mesh_uploaded(big), "does not fit");
        assert!(assets.contains_mesh(big));
        let (v, i) = (
            assets.arena.vertex.as_ref().unwrap(),
            assets.arena.index.as_ref().unwrap(),
        );
        assert!(v.size() <= r.caps.max_buffer_size && i.size() <= r.caps.max_buffer_size);
        assert_eq!(v.size(), 4096 * 36, "no growth for the small meshes");

        // Index-heavy meshes grow only the index buffer.
        let fan = MeshData {
            indices: (0..9000).map(|k| k % 3).collect(),
            ..MeshData::plane(1.0)
        };
        let m = assets.add_mesh(fan);
        r.upload_assets(&mut assets);
        assert!(assets.is_mesh_uploaded(m));
        assert_eq!(assets.arena.vertex.as_ref().unwrap().size(), 4096 * 36);
        assert!(assets.arena.index.as_ref().unwrap().size() > 8192 * 4);

        // The rejected mesh fits once the limit allows it.
        r.caps.max_buffer_size = u32::MAX as u64;
        assert!(assets.update_mesh(big, MeshData::sphere(1.0, 200, 100)));
        r.upload_assets(&mut assets);
        assert!(assets.is_mesh_uploaded(big));
        assert!(small.iter().all(|m| assets.is_mesh_uploaded(*m)));
        assert_eq!(r.gpu_error_count(), 0);
    }

    /// Regression: browser WebGPU canvases offer no sRGB format, so the renderer encoded sRGB in
    /// the shader and blending/MSAA resolves ran on encoded values (50% white over black gave
    /// 128 instead of 188). It now renders through an sRGB view of the canvas texture.
    #[test]
    fn surfaces_without_srgb_formats_render_through_an_srgb_view() {
        use wgpu::TextureFormat as F;
        let webgpu_canvas = [F::Bgra8Unorm, F::Rgba8Unorm, F::Rgba16Float];
        assert_eq!(
            surface_formats(&webgpu_canvas, true),
            Some((F::Bgra8Unorm, F::Bgra8UnormSrgb))
        );
        // Without SURFACE_VIEW_FORMATS: in-shader encoding.
        assert_eq!(
            surface_formats(&webgpu_canvas, false),
            Some((F::Bgra8Unorm, F::Bgra8Unorm))
        );
        // Native surfaces with an sRGB format use it directly.
        assert_eq!(
            surface_formats(&[F::Bgra8Unorm, F::Bgra8UnormSrgb], true),
            Some((F::Bgra8UnormSrgb, F::Bgra8UnormSrgb))
        );
        // Formats without an sRGB twin keep in-shader encoding.
        assert_eq!(
            surface_formats(&[F::Rgba16Float], true),
            Some((F::Rgba16Float, F::Rgba16Float))
        );
        assert_eq!(surface_formats(&[], true), None);
    }

    /// A canvas that rejects the sRGB view format (reported as `Lost` on the web) is
    /// reconfigured without it once, instead of being recreated with the same configuration
    /// in a loop. Native surfaces and surfaces that already presented are recreated.
    #[test]
    fn rejected_srgb_view_formats_fall_back_to_shader_encoding() {
        use wgpu::TextureFormat as F;
        let mut config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: F::Bgra8Unorm,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: 8,
            height: 8,
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: 2,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: vec![F::Bgra8UnormSrgb],
        };
        assert!(retry_without_view_formats(true, false, &config));
        assert!(!retry_without_view_formats(true, true, &config));
        assert!(!retry_without_view_formats(false, false, &config));
        config.view_formats.clear();
        assert!(!retry_without_view_formats(true, false, &config));
    }

    /// Without `DownlevelFlags::BASE_VERTEX` (WebGL2, GLES < 3.2) only `CpuDirect` is offered:
    /// it is the path whose direct draws emulate `base_vertex`.
    #[test]
    fn gpu_paths_require_base_vertex() {
        assert_eq!(best_draw_path(true, true, true, true), DrawPath::GpuMultiDrawCount);
        assert_eq!(best_draw_path(true, true, true, false), DrawPath::GpuMultiDraw);
        assert_eq!(best_draw_path(true, true, false, false), DrawPath::GpuIndirect);
        assert_eq!(best_draw_path(true, false, true, true), DrawPath::CpuDirect);
        assert_eq!(best_draw_path(false, true, true, true), DrawPath::CpuDirect);
    }

    fn invalid_buffer(device: &wgpu::Device) {
        // MAP_READ may only be combined with COPY_DST: a validation error.
        let _ = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("invalid on purpose"),
            size: 16,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });
    }

    /// Release builds log uncaptured wgpu errors instead of wgpu's default panic; debug builds
    /// (and tests) still panic so bugs surface.
    #[test]
    fn uncaptured_wgpu_errors_are_logged_in_release_and_panic_in_debug() {
        let Some(r) = renderer() else { return };
        assert_eq!(r.gpu_error_count(), 0);
        let count = install_error_handler(&r.device, false);
        invalid_buffer(&r.device);
        invalid_buffer(&r.device);
        assert_eq!(count.load(Ordering::Relaxed), 2, "logged, not panicked");

        let count = install_error_handler(&r.device, true);
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| invalid_buffer(&r.device)));
        assert!(panicked.is_err());
        assert_eq!(count.load(Ordering::Relaxed), 1);
    }
}
