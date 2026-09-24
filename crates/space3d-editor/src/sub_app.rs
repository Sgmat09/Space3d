//! Apps running inside the editor: [`SubApp`] storage and [`LiveView`] (a sub-app rendered
//! headless and streamed into an egui texture).
//!
//! # Panics
//!
//! A panic inside a sub-app is caught (`catch_unwind`) and reported through
//! [`LiveView::error`] only when panics unwind ([`panics_are_isolated`]): debug/test builds and
//! release builds with `panic = "unwind"`. With `panic = "abort"` — this workspace's release
//! profile, and always on `wasm32-unknown-unknown` — a panicking story aborts the whole viewer.
//! Run the storybook from a dev build to debug a crashing story.

use space3d_core::App;
use space3d_core::ImageData;
use space3d_core::glam::Vec2;
use space3d_platform::{Input, MouseButton};
use space3d_render::{RenderError, Renderer};
use space3d_tasks::Task;
use std::cell::RefCell;
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

thread_local! {
    static APPS: RefCell<HashMap<u64, App>> = RefCell::new(HashMap::new());
}
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Handle to an [`App`] owned by the current thread.
///
/// `App` is not `Send` (its runner is an arbitrary closure), so it cannot live in resources;
/// the handle can. The app is stored thread-locally and reachable through [`SubApp::with`] on
/// the thread that created it (the main thread, where all systems run). Dropping the handle
/// drops the app.
#[derive(Debug)]
pub struct SubApp {
    id: u64,
}

impl SubApp {
    /// Store `app` and return its handle.
    pub fn new(app: App) -> Self {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        APPS.with(|m| m.borrow_mut().insert(id, app));
        Self { id }
    }

    /// Run `f` with the app. Returns `None` if the app is gone (a previous `f` panicked, or
    /// called from another thread). Re-entrant: `f` may use other sub-apps.
    pub fn with<R>(&self, f: impl FnOnce(&mut App) -> R) -> Option<R> {
        let mut app = APPS.with(|m| m.borrow_mut().remove(&self.id))?;
        let out = f(&mut app);
        APPS.with(|m| m.borrow_mut().insert(self.id, app));
        Some(out)
    }

    /// Whether the app is still available.
    pub fn is_alive(&self) -> bool {
        APPS.with(|m| m.borrow().contains_key(&self.id))
    }
}

impl Drop for SubApp {
    fn drop(&mut self) {
        let app = APPS.try_with(|m| m.borrow_mut().remove(&self.id)).ok().flatten();
        drop(app);
    }
}

/// Whether panics inside sub-apps ([`LiveView`], storybook stories) are caught and reported.
///
/// True when panics unwind (debug/test builds, or `panic = "unwind"`). With
/// `panic = "abort"` — this workspace's release profile, and always on
/// `wasm32-unknown-unknown` — a panicking story aborts the whole viewer; run the storybook
/// from a dev build to debug it.
pub const fn panics_are_isolated() -> bool {
    cfg!(panic = "unwind")
}

/// Message of a caught panic.
pub(crate) fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "panic".to_owned()
    }
}

fn create_renderer(width: u32, height: u32) -> Task<Result<Renderer, RenderError>> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        space3d_tasks::spawn_compute(move || pollster::block_on(Renderer::new_headless(width, height)))
    }
    #[cfg(target_arch = "wasm32")]
    {
        space3d_tasks::spawn(web_renderer(width, height))
    }
}

/// Web: a headless renderer needs WebGPU. WebGL2 adapters only exist for a surface, so
/// without WebGPU the story renders to a hidden canvas of its own (captures read back the
/// offscreen copy as usual).
#[cfg(target_arch = "wasm32")]
async fn web_renderer(width: u32, height: u32) -> Result<Renderer, RenderError> {
    use wasm_bindgen::JsCast;
    let headless_err = match Renderer::new_headless(width, height).await {
        Ok(r) => return Ok(r),
        Err(e) => e,
    };
    let canvas: web_sys::HtmlCanvasElement = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.create_element("canvas").ok())
        .and_then(|e| e.dyn_into().ok())
        .ok_or_else(|| RenderError::Surface("cannot create a canvas".into()))?;
    canvas.set_width(width);
    canvas.set_height(height);
    let config = space3d_render::RendererConfig {
        width,
        height,
        vsync: false,
        ..Default::default()
    };
    Renderer::new_with_surface(space3d_render::wgpu::SurfaceTarget::Canvas(canvas), config)
        .await
        .map_err(|e| {
            RenderError::NoAdapter(format!(
                "story viewport needs WebGPU or WebGL2 (headless: {headless_err}; canvas: {e})"
            ))
        })
}

/// A sub-app with its own headless renderer whose frames are read back asynchronously and
/// shown as an egui texture (used by the storybook viewer).
///
/// The renderer is created in the background (on the web: WebGPU, else a hidden WebGL2
/// canvas), frames are captured with `Renderer::capture` (never blocking the host frame) and
/// the newest finished capture is displayed. Panics inside the sub-app are reported via
/// [`LiveView::error`] when panics unwind — see [`panics_are_isolated`].
pub struct LiveView {
    app: SubApp,
    size: (u32, u32),
    renderer: Option<Task<Result<Renderer, RenderError>>>,
    capture: Option<Task<Result<ImageData, RenderError>>>,
    texture: Option<egui::TextureHandle>,
    image: Option<ImageData>,
    error: Option<String>,
    frames: u64,
    hovered: bool,
}

impl std::fmt::Debug for LiveView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveView")
            .field("size", &self.size)
            .field("frames", &self.frames)
            .field("error", &self.error)
            .finish()
    }
}

impl LiveView {
    /// Wrap `app` (which should have `RenderPlugin` and no renderer yet) and start creating a
    /// `width`x`height` headless renderer for it.
    pub fn new(app: App, width: u32, height: u32) -> Self {
        Self {
            app: SubApp::new(app),
            size: (width.max(1), height.max(1)),
            renderer: Some(create_renderer(width.max(1), height.max(1))),
            capture: None,
            texture: None,
            image: None,
            error: None,
            frames: 0,
            hovered: false,
        }
    }

    /// Render size in pixels.
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// The sub-app (see [`SubApp::with`]).
    pub fn app(&self) -> &SubApp {
        &self.app
    }

    /// Error (panic in the sub-app, renderer creation failure...).
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Whether the renderer exists.
    pub fn has_renderer(&self) -> bool {
        self.renderer.is_none() && self.app.with(|a| a.resources.contains::<Renderer>()).unwrap_or(false)
    }

    /// Latest captured frame.
    pub fn image(&self) -> Option<&ImageData> {
        self.image.as_ref()
    }

    /// Frames captured so far.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// egui texture with the latest frame.
    pub fn texture_id(&self) -> Option<egui::TextureId> {
        self.texture.as_ref().map(egui::TextureHandle::id)
    }

    /// Run `f` on the sub-app, catching panics.
    pub fn run(&mut self, f: impl FnOnce(&mut App)) {
        if self.error.is_some() {
            return;
        }
        let result = self
            .app
            .with(|app| std::panic::catch_unwind(AssertUnwindSafe(|| f(app))));
        match result {
            Some(Ok(())) => {}
            Some(Err(payload)) => self.error = Some(panic_message(payload.as_ref())),
            None => self.error = Some("app unavailable".into()),
        }
    }

    /// Advance the sub-app by `dt` (zero keeps the simulation still but still renders).
    pub fn update(&mut self, dt: Duration) {
        self.run(|app| app.update(dt));
    }

    /// Run exactly `n` fixed ticks, rendering each (blocks for all of them; prefer
    /// [`Self::fast_forward`] from systems).
    pub fn step_ticks(&mut self, n: u64) {
        self.run(|app| app.step_ticks(n));
    }

    /// Run up to `max` fixed ticks without rendering them, stopping early once `budget` of
    /// wall-clock time is spent (at least one tick runs). Returns the ticks run. The next
    /// [`Self::update`] renders the result. Used to spread "step N ticks" over host frames.
    pub fn fast_forward(&mut self, max: u64, budget: Duration) -> u64 {
        let mut done = 0;
        if max == 0 {
            return 0;
        }
        self.run(|app| {
            // Without a renderer `Stage::Render` is a no-op: skip drawing intermediate ticks.
            let renderer = app.resources.remove::<Renderer>();
            let start = web_time::Instant::now();
            while done < max {
                app.step_ticks(1);
                done += 1;
                if start.elapsed() >= budget {
                    break;
                }
            }
            if let Some(r) = renderer {
                app.resources.insert(r);
            }
        });
        done
    }

    /// Install the renderer once created, request a capture of the last rendered frame if none
    /// is in flight, and upload finished captures to the egui texture. Call once per host frame
    /// after [`Self::update`].
    pub fn pump(&mut self, ctx: &egui::Context) {
        if let Some(task) = &mut self.renderer
            && let Some(result) = task.poll()
        {
            self.renderer = None;
            match result {
                Ok(r) => {
                    self.app.with(|a| {
                        a.insert_resource(r);
                    });
                }
                Err(e) => self.error = Some(format!("renderer: {e}")),
            }
        }
        if let Some(task) = &mut self.capture
            && let Some(result) = task.poll()
        {
            self.capture = None;
            match result {
                Ok(img) => {
                    let shown = crate::texture_editor::display_image(&img, ctx.input(|i| i.max_texture_side));
                    let color = egui::ColorImage::from_rgba_unmultiplied(
                        [shown.width as usize, shown.height as usize],
                        &shown.pixels,
                    );
                    drop(shown);
                    match &mut self.texture {
                        Some(t) => t.set(color, egui::TextureOptions::LINEAR),
                        None => {
                            self.texture =
                                Some(ctx.load_texture("space3d live view", color, egui::TextureOptions::LINEAR))
                        }
                    }
                    self.image = Some(img);
                    self.frames += 1;
                }
                Err(e) => tracing::warn!("live view capture failed: {e}"),
            }
        }
        if self.capture.is_none() && self.renderer.is_none() && self.error.is_none() {
            self.capture = self
                .app
                .with(|a| a.resources.get_mut::<Renderer>().map(Renderer::capture))
                .flatten();
            if self.capture.is_some() {
                // Map callbacks fire on device polls (WebGL / idle native devices).
                self.app.with(|a| {
                    if let Some(r) = a.resources.get::<Renderer>() {
                        r.poll();
                    }
                });
            }
        }
    }

    /// Show the latest frame scaled into `max` (keeping the aspect ratio) and forward pointer
    /// input over it to the sub-app's `Input`. Returns the image response.
    pub fn show(&mut self, ui: &mut egui::Ui, max: egui::Vec2) -> egui::Response {
        let (w, h) = (self.size.0 as f32, self.size.1 as f32);
        let scale = (max.x / w).min(max.y / h).max(0.05);
        let size = egui::vec2(w * scale, h * scale);
        let response = match self.texture_id() {
            Some(id) => {
                ui.add(egui::Image::new(egui::load::SizedTexture::new(id, size)).sense(egui::Sense::click_and_drag()))
            }
            None => {
                let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
                ui.painter().rect_filled(rect, 0.0, egui::Color32::from_gray(20));
                let msg = match (&self.error, self.renderer.is_some()) {
                    (Some(e), _) => e.clone(),
                    (None, true) => "creating renderer...".to_owned(),
                    (None, false) => "waiting for first frame...".to_owned(),
                };
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    msg,
                    egui::FontId::proportional(14.0),
                    egui::Color32::GRAY,
                );
                response
            }
        };
        self.forward_input(ui, &response, scale);
        response
    }

    fn forward_input(&mut self, ui: &egui::Ui, response: &egui::Response, scale: f32) {
        let rect = response.rect;
        let hover = response.hover_pos();
        let (pressed, released, wheel, text) = ui.input(|i| {
            let mut text = String::new();
            for e in &i.events {
                if let egui::Event::Text(t) = e {
                    text.push_str(t);
                }
            }
            (
                [
                    i.pointer.button_pressed(egui::PointerButton::Primary),
                    i.pointer.button_pressed(egui::PointerButton::Secondary),
                    i.pointer.button_pressed(egui::PointerButton::Middle),
                ],
                [
                    i.pointer.button_released(egui::PointerButton::Primary),
                    i.pointer.button_released(egui::PointerButton::Secondary),
                    i.pointer.button_released(egui::PointerButton::Middle),
                ],
                i.smooth_scroll_delta,
                text,
            )
        });
        let buttons = [MouseButton::Left, MouseButton::Right, MouseButton::Middle];
        let dragging = response.dragged();
        let was_hovered = self.hovered;
        self.hovered = hover.is_some() || dragging;
        let pos = ui.input(|i| i.pointer.interact_pos());
        self.app.with(|app| {
            let Some(input) = app.resources.get_mut::<Input>() else {
                return;
            };
            if let Some(p) = pos.filter(|_| hover.is_some() || dragging) {
                let local = (p - rect.min) / scale;
                input.inject_mouse_move(Vec2::new(local.x, local.y));
                for (i, b) in buttons.into_iter().enumerate() {
                    if pressed[i] && hover.is_some() {
                        input.inject_mouse_button_down(b);
                    }
                }
                if wheel != egui::Vec2::ZERO && hover.is_some() {
                    input.inject_wheel(Vec2::new(wheel.x, wheel.y) / 40.0);
                }
                if !text.is_empty() {
                    input.inject_text(&text);
                }
            } else if was_hovered {
                input.inject_mouse_left();
            }
            for (i, b) in buttons.into_iter().enumerate() {
                if released[i] && input.mouse_pressed(b) {
                    input.inject_mouse_button_up(b);
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sub_apps_are_reentrant_and_dropped() {
        let outer = SubApp::new(App::new());
        let inner = SubApp::new(App::new());
        let n = outer
            .with(|a| {
                a.step_ticks(2);
                inner.with(|b| {
                    b.step_ticks(1);
                    b.resources.expect::<space3d_core::Time>().tick
                })
            })
            .flatten();
        assert_eq!(n, Some(1));
        assert!(outer.is_alive());
        let id = outer.id;
        drop(outer);
        assert!(APPS.with(|m| !m.borrow().contains_key(&id)));
    }

    #[test]
    fn fast_forward_skips_rendering_and_respects_the_budget() {
        #[derive(Default)]
        struct Ticks(u64);
        let mut app = App::new();
        app.init_resource::<Ticks>();
        app.add_system(space3d_core::Stage::FixedUpdate, |c| {
            c.resources.expect_mut::<Ticks>().0 += 1
        });
        let mut view = LiveView {
            app: SubApp::new(app),
            size: (4, 4),
            renderer: None,
            capture: None,
            texture: None,
            image: None,
            error: None,
            frames: 0,
            hovered: false,
        };
        assert_eq!(view.fast_forward(0, Duration::from_secs(1)), 0);
        assert_eq!(view.fast_forward(5, Duration::from_secs(10)), 5);
        // A zero budget still makes progress, one tick per call.
        assert_eq!(view.fast_forward(5, Duration::ZERO), 1);
        let ticks = view.app().with(|a| a.resources.expect::<Ticks>().0).unwrap();
        assert_eq!(ticks, 6);
    }

    #[test]
    fn live_view_catches_panics() {
        if !panics_are_isolated() {
            return;
        }
        let mut app = App::new();
        app.add_system(space3d_core::Stage::Update, |_| panic!("story exploded"));
        let mut view = LiveView {
            app: SubApp::new(app),
            size: (4, 4),
            renderer: None,
            capture: None,
            texture: None,
            image: None,
            error: None,
            frames: 0,
            hovered: false,
        };
        view.update(Duration::from_millis(16));
        assert_eq!(view.error(), Some("story exploded"));
        // Further updates are ignored instead of panicking again.
        view.update(Duration::from_millis(16));
    }
}
