//! The winit 0.30 `ApplicationHandler` runner shared by desktop, web, Android and iOS.
//!
//! Lifecycle:
//! * `resumed` — create the window once; create the [`Renderer`] (blocking on native,
//!   asynchronously on the web) or, on mobile resume, recreate its surface.
//! * `suspended` — drop the surface (Android destroys the native window in background).
//! * `window_event` — feed [`Input`], track size/DPI/focus, render on `RedrawRequested`.
//! * `about_to_wait` — request the next redraw (continuous rendering).

use crate::input::{Input, MouseButton, TouchPhase};
use crate::window::{PlatformEvent, WindowPlugin, WindowState};
use glam::{UVec2, Vec2};
use space3d_core::{App, Events};
use space3d_render::{Renderer, RendererConfig};
use std::sync::Arc;
use std::time::Duration;
use web_time::Instant;
use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::PhysicalKey;
use winit::window::{CursorGrabMode, Window, WindowId};

/// Pixels per "line" when converting pixel-precise scrolling (trackpads) to lines.
const PIXELS_PER_LINE: f32 = 40.0;

pub(crate) struct WinitRunner {
    app: App,
    settings: WindowPlugin,
    window: Option<Arc<Window>>,
    last_frame: Option<Instant>,
    renderer_requested: bool,
    raw_mouse_motion: bool,
    exiting: bool,
    applied_title: String,
    applied_cursor: (bool, bool),
    #[cfg(target_arch = "wasm32")]
    pending: std::rc::Rc<std::cell::RefCell<Option<Result<Renderer, space3d_render::RenderError>>>>,
}

fn build_event_loop() -> Result<EventLoop<()>, winit::error::EventLoopError> {
    #[cfg(target_os = "android")]
    {
        use winit::platform::android::EventLoopBuilderExtAndroid;
        let Some(android_app) = crate::mobile::android_app() else {
            tracing::error!("Android: start the app through space3d_platform::android_main_entry");
            return Err(winit::error::EventLoopError::NotSupported(
                winit::error::NotSupportedError::new(),
            ));
        };
        EventLoop::builder().with_android_app(android_app).build()
    }
    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
        let mut builder = EventLoop::builder();
        if crate::background::background_mode() {
            // Never come to front / take focus: no Dock icon, no activation on launch.
            builder
                .with_activation_policy(ActivationPolicy::Accessory)
                .with_activate_ignoring_other_apps(false);
        }
        builder.build()
    }
    #[cfg(not(any(target_os = "android", target_os = "macos")))]
    {
        EventLoop::new()
    }
}

/// Runner installed by [`WindowPlugin`]. Falls back to the headless runner when no event loop
/// can be created (e.g. CI without a display server).
pub(crate) fn run(app: App, settings: WindowPlugin) {
    let event_loop = match build_event_loop() {
        Ok(el) => el,
        Err(e) => {
            tracing::error!("cannot create event loop ({e}); running headless");
            space3d_core::app::headless_runner(app);
            return;
        }
    };
    event_loop.set_control_flow(ControlFlow::Poll);
    let runner = WinitRunner {
        applied_title: settings.title.clone(),
        app,
        settings,
        window: None,
        last_frame: None,
        renderer_requested: false,
        raw_mouse_motion: false,
        exiting: false,
        applied_cursor: (true, false),
        #[cfg(target_arch = "wasm32")]
        pending: Default::default(),
    };
    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut runner = runner;
        if let Err(e) = event_loop.run_app(&mut runner) {
            tracing::error!("event loop error: {e}");
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        use winit::platform::web::EventLoopExtWebSys;
        // Returns immediately; the browser drives the loop.
        event_loop.spawn_app(runner);
    }
}

impl WinitRunner {
    fn send(&mut self, event: PlatformEvent) {
        self.app.resources.init::<Events<PlatformEvent>>().send(event);
    }

    fn input(&mut self) -> &mut Input {
        self.app.resources.init::<Input>()
    }

    fn state(&mut self) -> &mut WindowState {
        self.app.resources.expect_mut::<WindowState>()
    }

    fn renderer_config(&self, size: UVec2) -> RendererConfig {
        RendererConfig {
            width: size.x.max(1),
            height: size.y.max(1),
            msaa_samples: self.settings.msaa_samples,
            vsync: self.settings.vsync,
            ..Default::default()
        }
    }

    fn create_window(&mut self, event_loop: &ActiveEventLoop) -> Option<Arc<Window>> {
        let s = &self.settings;
        #[allow(unused_mut)]
        let mut attrs = Window::default_attributes()
            .with_title(s.title.clone())
            .with_inner_size(winit::dpi::LogicalSize::new(s.width as f64, s.height as f64))
            .with_resizable(s.resizable);
        if crate::background::background_mode() {
            attrs = attrs.with_active(false);
        }
        #[cfg(target_arch = "wasm32")]
        {
            attrs = crate::web::window_attributes(attrs, s.canvas_id.as_deref());
        }
        match event_loop.create_window(attrs) {
            Ok(w) => Some(Arc::new(w)),
            Err(e) => {
                tracing::error!("failed to create window: {e}");
                None
            }
        }
    }

    fn on_resize(&mut self, size: UVec2) {
        let size = size.max(UVec2::ONE);
        self.state().physical_size = size;
        self.input().set_window_size(size.as_vec2());
        if let Some(r) = self.app.resources.get_mut::<Renderer>() {
            r.resize(size.x, size.y);
        }
        self.send(PlatformEvent::Resized(size));
    }

    /// Create the renderer on first resume, or reattach the surface on later resumes.
    fn ensure_renderer(&mut self, event_loop: &ActiveEventLoop, window: &Arc<Window>) {
        let size = window.inner_size();
        let size = UVec2::new(size.width, size.height).max(UVec2::ONE);
        if let Some(r) = self.app.resources.get_mut::<Renderer>() {
            if !r.has_surface() {
                #[cfg(not(target_arch = "wasm32"))]
                let result = r.set_surface(window.clone());
                #[cfg(target_arch = "wasm32")]
                let result = match crate::web::canvas_of(window) {
                    Some(c) => r.set_surface(space3d_render::wgpu::SurfaceTarget::Canvas(c)),
                    None => Err(space3d_render::RenderError::Surface("window has no canvas".into())),
                };
                match result {
                    Ok(()) => r.resize(size.x, size.y),
                    Err(e) => tracing::error!("failed to recreate surface: {e}"),
                }
            }
            return;
        }
        if self.renderer_requested {
            return;
        }
        self.renderer_requested = true;
        let config = self.renderer_config(size);
        #[cfg(not(target_arch = "wasm32"))]
        {
            let display: space3d_render::DisplayHandle = Box::new(event_loop.owned_display_handle());
            match pollster::block_on(Renderer::new_with_surface_and_display(
                window.clone(),
                Some(display),
                config,
            )) {
                Ok(r) => {
                    self.app.insert_resource(r);
                }
                Err(e) => {
                    tracing::error!("failed to create renderer: {e}");
                    event_loop.exit();
                }
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = event_loop;
            let Some(canvas) = crate::web::canvas_of(window) else {
                tracing::error!("window has no canvas");
                return;
            };
            let pending = self.pending.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let target = space3d_render::wgpu::SurfaceTarget::Canvas(canvas);
                let result = Renderer::new_with_surface(target, config).await;
                *pending.borrow_mut() = Some(result);
            });
        }
    }

    /// Web: install the renderer once its async creation finished.
    #[cfg(target_arch = "wasm32")]
    fn poll_pending_renderer(&mut self) {
        let Some(result) = self.pending.borrow_mut().take() else {
            return;
        };
        match result {
            Ok(mut r) => {
                if let Some(w) = &self.window {
                    let s = w.inner_size();
                    r.resize(s.width.max(1), s.height.max(1));
                }
                self.app.insert_resource(r);
            }
            Err(e) => tracing::error!("failed to create renderer: {e}"),
        }
    }

    fn frame(&mut self, event_loop: &ActiveEventLoop) {
        #[cfg(target_arch = "wasm32")]
        self.poll_pending_renderer();
        if self.exiting || self.state().suspended {
            return;
        }
        let now = Instant::now();
        let dt = self.last_frame.map_or(Duration::ZERO, |last| now - last);
        self.last_frame = Some(now);
        if let Some(w) = &self.window {
            w.pre_present_notify();
        }
        self.app.update(dt);
        self.apply_window_state();
        if self.app.should_exit() {
            self.exiting = true;
            event_loop.exit();
        }
    }

    /// Push game-side changes of [`WindowState`] to the OS window.
    fn apply_window_state(&mut self) {
        let Some(window) = self.window.clone() else {
            return;
        };
        let (title, cursor) = {
            let s = self.state();
            (s.title.clone(), (s.cursor_visible, s.cursor_grab))
        };
        if title != self.applied_title {
            window.set_title(&title);
            self.applied_title = title;
        }
        if cursor != self.applied_cursor {
            window.set_cursor_visible(cursor.0);
            let mode = if cursor.1 {
                CursorGrabMode::Locked
            } else {
                CursorGrabMode::None
            };
            if window.set_cursor_grab(mode).is_err() && cursor.1 {
                let _ = window.set_cursor_grab(CursorGrabMode::Confined);
            }
            self.applied_cursor = cursor;
        }
    }
}

impl ApplicationHandler for WinitRunner {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() {
            let Some(w) = self.create_window(event_loop) else {
                event_loop.exit();
                return;
            };
            self.window = Some(w);
        }
        let window = self.window.clone().expect("window");
        let scale = window.scale_factor();
        {
            let s = self.state();
            s.suspended = false;
            s.scale_factor = scale;
        }
        self.ensure_renderer(event_loop, &window);
        let size = window.inner_size();
        self.on_resize(UVec2::new(size.width, size.height));
        self.send(PlatformEvent::Resumed);
        // Avoid a huge dt after returning from background.
        self.last_frame = None;
        window.request_redraw();
    }

    fn suspended(&mut self, _event_loop: &ActiveEventLoop) {
        // Android destroys the native window while in background: drop the surface now and
        // recreate it in `resumed`. Harmless elsewhere.
        if let Some(r) = self.app.resources.get_mut::<Renderer>() {
            r.remove_surface();
        }
        self.state().suspended = true;
        self.input().release_all();
        self.send(PlatformEvent::Suspended);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                self.send(PlatformEvent::CloseRequested);
                if self.settings.exit_on_close {
                    self.exiting = true;
                    event_loop.exit();
                }
            }
            WindowEvent::Resized(size) => self.on_resize(UVec2::new(size.width, size.height)),
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.state().scale_factor = scale_factor;
                self.send(PlatformEvent::ScaleFactorChanged(scale_factor));
            }
            WindowEvent::Focused(focused) => {
                self.state().focused = focused;
                if !focused {
                    self.input().release_all();
                }
                self.send(PlatformEvent::Focused(focused));
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let pressed = event.state == ElementState::Pressed;
                if let PhysicalKey::Code(code) = event.physical_key {
                    if pressed {
                        self.input().inject_key_down(code);
                    } else {
                        self.input().inject_key_up(code);
                    }
                }
                if pressed && let Some(text) = &event.text {
                    self.input().inject_text(text);
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let b = MouseButton::from(button);
                match state {
                    ElementState::Pressed => self.input().inject_mouse_button_down(b),
                    ElementState::Released => self.input().inject_mouse_button_up(b),
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let p = Vec2::new(position.x as f32, position.y as f32);
                if self.raw_mouse_motion {
                    self.input().set_mouse_position(Some(p));
                } else {
                    self.input().inject_mouse_move(p);
                }
            }
            WindowEvent::CursorLeft { .. } => self.input().inject_mouse_left(),
            WindowEvent::MouseWheel { delta, .. } => {
                let d = match delta {
                    MouseScrollDelta::LineDelta(x, y) => Vec2::new(x, y),
                    MouseScrollDelta::PixelDelta(p) => Vec2::new(p.x as f32, p.y as f32) / PIXELS_PER_LINE,
                };
                self.input().inject_wheel(d);
            }
            WindowEvent::Touch(t) => {
                let p = Vec2::new(t.location.x as f32, t.location.y as f32);
                self.input().inject_touch(t.id, TouchPhase::from(t.phase), p);
            }
            WindowEvent::RedrawRequested => self.frame(event_loop),
            _ => {}
        }
    }

    fn device_event(&mut self, _event_loop: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        if let DeviceEvent::MouseMotion { delta } = event {
            // Raw motion is unaccelerated and keeps working with a locked cursor; once seen,
            // cursor moves only update the position.
            self.raw_mouse_motion = true;
            self.input()
                .inject_mouse_motion(Vec2::new(delta.0 as f32, delta.1 as f32));
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        #[cfg(target_arch = "wasm32")]
        self.poll_pending_renderer();
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }
}
