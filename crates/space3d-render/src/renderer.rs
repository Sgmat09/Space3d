//! [`Renderer`]: device/queue ownership, surfaces, render targets, asset upload and capture.

use crate::assets::RenderAssets;
use crate::frame::FrameState;
use crate::gpu::{Uploader, align_up};
use crate::pipeline::{DEPTH_FORMAT, OFFSCREEN_FORMAT, Pipelines};
use crate::stats::{DrawPath, RenderStats};
use crate::texture::{SamplerCache, create_gpu_texture, format_supported, white_texture};
use space3d_core::ImageData;
use space3d_tasks::Task;

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
    /// BC / ASTC / ETC2 compressed textures.
    pub texture_compression: (bool, bool, bool),
    /// Best draw path for this device.
    pub best_draw_path: DrawPath,
    /// MSAA sample counts usable on the primary target.
    pub max_msaa: u32,
    /// Largest storage binding (bounds GPU-culled buffers).
    pub max_storage_binding: u64,
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
        let ifi = features.contains(wgpu::Features::INDIRECT_FIRST_INSTANCE);
        let mdic = features.contains(wgpu::Features::MULTI_DRAW_INDIRECT_COUNT);
        let best_draw_path = match (compute && indirect, ifi, mdic) {
            (true, true, true) => DrawPath::GpuMultiDrawCount,
            (true, true, false) => DrawPath::GpuMultiDraw,
            (true, false, _) => DrawPath::GpuIndirect,
            _ => DrawPath::CpuDirect,
        };
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
            texture_compression: (
                features.contains(wgpu::Features::TEXTURE_COMPRESSION_BC),
                features.contains(wgpu::Features::TEXTURE_COMPRESSION_ASTC),
                features.contains(wgpu::Features::TEXTURE_COMPRESSION_ETC2),
            ),
            best_draw_path,
            max_msaa: if msaa4 { 4 } else { 1 },
            max_storage_binding: limits.max_storage_buffer_binding_size,
        }
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
    pub fn new(device: &wgpu::Device, width: u32, height: u32, samples: u32, format: wgpu::TextureFormat, offscreen: bool) -> Self {
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
                wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::TEXTURE_BINDING,
            )
        });
        let msaa_view = (samples > 1).then(|| {
            tex("space3d msaa color", format, samples, wgpu::TextureUsages::RENDER_ATTACHMENT).create_view(&Default::default())
        });
        let depth = tex("space3d depth", DEPTH_FORMAT, samples, wgpu::TextureUsages::RENDER_ATTACHMENT);
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
    pub manual_srgb: bool,
    pub attachments: Attachments,
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
            | wgpu::Features::TEXTURE_COMPRESSION_BC
            | wgpu::Features::TEXTURE_COMPRESSION_ASTC
            | wgpu::Features::TEXTURE_COMPRESSION_ETC2;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("space3d device"),
                required_features: adapter.features() & wanted,
                required_limits: adapter.limits(),
                ..Default::default()
            })
            .await
            .map_err(|e| RenderError::Device(e.to_string()))?;
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
        let offscreen = Attachments::new(&device, config.width, config.height, config.msaa_samples, OFFSCREEN_FORMAT, true);
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
        };
        if let Some(surface) = surface {
            renderer.configure_surface(surface)?;
        }
        Ok(renderer)
    }

    fn configure_surface(&mut self, surface: wgpu::Surface<'static>) -> Result<(), RenderError> {
        let caps = surface.get_capabilities(&self.adapter);
        let Some(&first) = caps.formats.first() else {
            return Err(RenderError::Surface("surface not supported by adapter".into()));
        };
        let format = caps.formats.iter().copied().find(|f| f.is_srgb()).unwrap_or(first);
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
            alpha_mode: caps.alpha_modes.first().copied().unwrap_or(wgpu::CompositeAlphaMode::Auto),
            view_formats: vec![],
        };
        surface.configure(&self.device, &config);
        let samples = if self.config.msaa_samples > 1
            && self.adapter.get_texture_format_features(format).flags.sample_count_supported(self.config.msaa_samples)
        {
            self.config.msaa_samples
        } else {
            1
        };
        let attachments = Attachments::new(&self.device, config.width, config.height, samples, format, false);
        self.surface = Some(SurfaceState {
            surface,
            manual_srgb: !format.is_srgb(),
            config,
            attachments,
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

    /// Resize the primary target (surface and offscreen capture target).
    pub fn resize(&mut self, width: u32, height: u32) {
        let (width, height) = (width.max(1), height.max(1));
        self.config.width = width;
        self.config.height = height;
        if let Some(s) = &mut self.surface {
            s.config.width = width;
            s.config.height = height;
            s.surface.configure(&self.device, &s.config);
            s.attachments = Attachments::new(&self.device, width, height, s.attachments.samples, s.config.format, false);
        }
        if (self.offscreen.width, self.offscreen.height) != (width, height) {
            self.offscreen = Attachments::new(&self.device, width, height, self.offscreen.samples, OFFSCREEN_FORMAT, true);
        }
    }

    /// Change MSAA sample count (1 or 4; clamped to device support). Recreates attachments.
    pub fn set_msaa_samples(&mut self, samples: u32) {
        let samples = if samples > 1 { self.caps.max_msaa } else { 1 };
        if samples == self.config.msaa_samples {
            return;
        }
        self.config.msaa_samples = samples;
        let (w, h) = (self.config.width, self.config.height);
        self.offscreen = Attachments::new(&self.device, w, h, samples, OFFSCREEN_FORMAT, true);
        if let Some(s) = &mut self.surface {
            let ok = self
                .adapter
                .get_texture_format_features(s.config.format)
                .flags
                .sample_count_supported(samples);
            let samples = if ok { samples } else { 1 };
            s.attachments = Attachments::new(&self.device, s.config.width, s.config.height, samples, s.config.format, false);
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

    /// Surface format when windowed.
    pub fn surface_format(&self) -> Option<wgpu::TextureFormat> {
        self.surface.as_ref().map(|s| s.config.format)
    }

    /// Whether textures of `format` can be uploaded (compressed formats need device features).
    pub fn supports_texture_format(&self, format: wgpu::TextureFormat) -> bool {
        format_supported(&self.device, format)
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
                match create_gpu_texture(&self.device, &self.queue, encoder, &mut self.pipelines, &mut self.samplers, entry) {
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
        // Convert + allocate.
        let scratch = &mut self.frame.mesh_scratch;
        scratch.clear();
        let mut missing = (0u32, 0u32);
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
            // Reuse the existing allocation when the size is unchanged.
            let alloc = match entry.gpu {
                Some(g) if g.alloc.vertex_count == vc && g.alloc.index_count == ic => Some(g.alloc),
                Some(g) => {
                    assets.arena.free(g.alloc);
                    entry.gpu = None;
                    assets.arena.alloc(vc, ic)
                }
                None => assets.arena.alloc(vc, ic),
            };
            if alloc.is_none() {
                missing.0 += vc;
                missing.1 += ic;
            }
            scratch.push((h, alloc, v, i));
        }
        let mut fresh = (u32::MAX, u32::MAX);
        if missing != (0, 0) || assets.arena.vertex.is_none() {
            let old = assets.arena.grow_capacity(missing.0, missing.1);
            for (_, alloc, v, i) in scratch.iter_mut() {
                if alloc.is_none() {
                    *alloc = assets.arena.alloc(v.len() as u32, i.len() as u32);
                    debug_assert!(alloc.is_some(), "arena growth must satisfy pending meshes");
                }
            }
            let writes: Vec<_> = scratch
                .iter()
                .filter_map(|(_, a, v, i)| {
                    a.map(|alloc| crate::arena::ArenaWrite {
                        alloc,
                        vertices: v,
                        indices: i,
                    })
                })
                .collect();
            assets.arena.realloc_gpu(&self.device, encoder, old, &writes, &mut self.uploader);
            fresh = old;
        }
        for (h, alloc, v, i) in scratch.drain(..) {
            let Some(alloc) = alloc else {
                continue;
            };
            assets.arena.write(
                encoder,
                &crate::arena::ArenaWrite {
                    alloc,
                    vertices: &v,
                    indices: &i,
                },
                fresh.0,
                fresh.1,
                &mut self.uploader,
            );
            if let Some(entry) = assets.meshes.get_mut(h.raw()) {
                entry.gpu = Some(crate::assets::GpuMesh {
                    alloc,
                    sphere: entry.sphere,
                });
                self.stats.meshes_uploaded += 1;
            }
        }
    }

    /// Read back the primary target of the last rendered frame as sRGB RGBA8.
    ///
    /// Headless: copies the offscreen target. Windowed: re-renders the last frame's primary
    /// views into the offscreen target first (surface textures are not readable).
    /// Poll the returned task from a system, `.block()` it in tests, or `.wait().await` it.
    pub fn capture(&mut self) -> Task<Result<ImageData, RenderError>> {
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
        self.queue.submit([encoder.finish()]);

        let read = move |buffer: wgpu::Buffer, ok: Result<(), wgpu::BufferAsyncError>| -> Result<ImageData, RenderError> {
            ok.map_err(|e| RenderError::Capture(e.to_string()))?;
            let view = buffer
                .slice(..)
                .get_mapped_range()
                .map_err(|e| RenderError::Capture(format!("{e:?}")))?;
            let row = w as usize * 4;
            let mut pixels = Vec::with_capacity(row * h as usize);
            for y in 0..h as usize {
                let start = y * padded as usize;
                pixels.extend_from_slice(&view[start..start + row]);
            }
            drop(view);
            buffer.unmap();
            Ok(ImageData {
                width: w,
                height: h,
                pixels,
            })
        };

        #[cfg(not(target_arch = "wasm32"))]
        {
            let device = self.device.clone();
            space3d_tasks::spawn_compute(move || {
                // Off the main thread: wait for the GPU, which fires the map callback.
                let _ = device.poll(wgpu::PollType::wait_indefinitely());
                match futures::executor::block_on(rx) {
                    Ok(r) => read(buffer, r),
                    Err(_) => Err(RenderError::Capture("map callback dropped".into())),
                }
            })
        }
        #[cfg(target_arch = "wasm32")]
        {
            // The browser (WebGPU) or the per-frame device poll (WebGL) resolves the mapping.
            space3d_tasks::spawn(async move {
                match rx.await {
                    Ok(r) => read(buffer, r),
                    Err(_) => Err(RenderError::Capture("map callback dropped".into())),
                }
            })
        }
    }

    /// [`Self::capture`] and wait for the result (tests/tools only; never from a system).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn capture_blocking(&mut self) -> Result<ImageData, RenderError> {
        self.capture()
            .block()
            .unwrap_or_else(|| Err(RenderError::Capture("capture task cancelled".into())))
    }

    /// Non-blocking device poll: fires pending map callbacks (captures, staging recycling).
    pub fn poll(&self) {
        let _ = self.device.poll(wgpu::PollType::Poll);
    }
}
