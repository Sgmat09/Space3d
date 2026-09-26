//! Texture editor panel: paint tools, adjustments and generators on top of
//! `space3d_assets::edit::texture`, with undo/redo through `ImageEditHistory`.
//!
//! Whole-image operations (filters, resize, generators...) run on the compute pool
//! ([`TextureEditor::apply_async`] / [`TextureEditor::replace_async`]) and are committed as one
//! undo step when done; the editor is read-only meanwhile. Decoding and PNG encoding also run
//! in the background. Images larger than the GPU's texture limit stay editable at full
//! resolution; only the on-screen copy is downscaled.

use crate::files::{IoJobs, read_file_with, write_file_with};
use crate::widgets::{fill_drag, fill_slider, property, section};
use space3d_assets::edit::texture::{self as tex, Blur, Brush, Filter, ImageEditHistory, Rect as PxRect};
use space3d_core::ImageData;
use space3d_tasks::Task;
use std::borrow::Cow;

/// A deferred operation on the editor (built inside UI closures, run afterwards).
type EditorOp = Box<dyn FnOnce(&mut TextureEditor)>;

/// Canvas tools.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TextureTool {
    /// Round brush.
    Brush,
    /// Round eraser (reduces alpha).
    Eraser,
    /// Flood fill.
    Fill,
    /// Straight line.
    Line,
    /// Rectangle (outline or filled).
    Rect,
    /// Eyedropper.
    Picker,
}

impl TextureTool {
    const ALL: [TextureTool; 6] = [
        TextureTool::Brush,
        TextureTool::Eraser,
        TextureTool::Fill,
        TextureTool::Line,
        TextureTool::Rect,
        TextureTool::Picker,
    ];

    fn label(self) -> &'static str {
        match self {
            TextureTool::Brush => "Brush",
            TextureTool::Eraser => "Eraser",
            TextureTool::Fill => "Fill",
            TextureTool::Line => "Line",
            TextureTool::Rect => "Rect",
            TextureTool::Picker => "Picker",
        }
    }
}

/// Parameters of the adjustment operations.
#[derive(Clone, Debug, PartialEq)]
pub struct Adjustments {
    /// Brightness offset (-1..1).
    pub brightness: f32,
    /// Contrast factor (0..3, 1 = unchanged).
    pub contrast: f32,
    /// Hue rotation in degrees.
    pub hue: f32,
    /// Saturation factor (1 = unchanged).
    pub saturation: f32,
    /// Lightness offset.
    pub lightness: f32,
    /// Gaussian blur sigma in pixels.
    pub blur_sigma: f32,
    /// Resize target.
    pub resize: [u32; 2],
    /// Noise seed.
    pub noise_seed: u64,
    /// Noise feature size in pixels.
    pub noise_scale: f32,
    /// Noise octaves.
    pub noise_octaves: u32,
    /// Normal-map strength.
    pub normal_strength: f32,
}

impl Default for Adjustments {
    fn default() -> Self {
        Self {
            brightness: 0.0,
            contrast: 1.0,
            hue: 0.0,
            saturation: 1.0,
            lightness: 0.0,
            blur_sigma: 2.0,
            resize: [256, 256],
            noise_seed: 1,
            noise_scale: 32.0,
            noise_octaves: 4,
            normal_strength: 2.0,
        }
    }
}

/// A whole-image operation running in the background.
struct ImageJob {
    label: String,
    task: Task<ImageData>,
}

/// The image to upload for display: `img` itself, or a copy downscaled (keeping the aspect
/// ratio) so neither side exceeds `max_side` — egui and wgpu reject larger textures.
pub fn display_image(img: &ImageData, max_side: usize) -> Cow<'_, ImageData> {
    let max_side = max_side.max(1) as u32;
    if img.width <= max_side && img.height <= max_side {
        return Cow::Borrowed(img);
    }
    let scale = max_side as f64 / img.width.max(img.height) as f64;
    let w = ((img.width as f64 * scale).round() as u32).clamp(1, max_side);
    let h = ((img.height as f64 * scale).round() as u32).clamp(1, max_side);
    Cow::Owned(tex::resize(img, w, h, Filter::Bilinear))
}

/// Texture editor state (resource inserted by `EditorPlugin`).
pub struct TextureEditor {
    /// Window shown.
    pub open: bool,
    /// Current tool.
    pub tool: TextureTool,
    /// Paint color (straight RGBA).
    pub color: [u8; 4],
    /// Brush / eraser settings (color taken from [`Self::color`]).
    pub brush: Brush,
    /// Flood-fill tolerance.
    pub fill_tolerance: u8,
    /// Fill rectangles.
    pub rect_filled: bool,
    /// Adjustment parameters.
    pub adjust: Adjustments,
    /// File path for open/save.
    pub path: String,
    /// Size for "New".
    pub new_size: [u32; 2],
    /// Set when the user asked to use the image on the selected entity (consumed by the editor).
    pub apply_to_selection: bool,
    history: Option<ImageEditHistory>,
    texture: Option<egui::TextureHandle>,
    dirty: bool,
    revision: u64,
    max_texture_side: usize,
    job: Option<ImageJob>,
    zoom: f32,
    fit_pending: bool,
    pan: egui::Vec2,
    stroke_last: Option<(f32, f32)>,
    drag_start: Option<(f32, f32)>,
    status: String,
    reads: IoJobs<ImageData>,
    writes: IoJobs<String>,
}

impl Default for TextureEditor {
    fn default() -> Self {
        Self {
            open: false,
            tool: TextureTool::Brush,
            color: [230, 60, 60, 255],
            brush: Brush {
                radius: 6.0,
                ..Default::default()
            },
            fill_tolerance: 16,
            rect_filled: false,
            adjust: Adjustments::default(),
            path: "texture.png".into(),
            new_size: [256, 256],
            apply_to_selection: false,
            history: None,
            texture: None,
            dirty: false,
            revision: 0,
            max_texture_side: 2048,
            job: None,
            zoom: 1.0,
            fit_pending: true,
            pan: egui::Vec2::ZERO,
            stroke_last: None,
            drag_start: None,
            status: String::new(),
            reads: IoJobs::default(),
            writes: IoJobs::default(),
        }
    }
}

impl std::fmt::Debug for TextureEditor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextureEditor")
            .field("open", &self.open)
            .field("tool", &self.tool)
            .field("size", &self.image().map(|i| (i.width, i.height)))
            .finish()
    }
}

/// Reduce alpha under a brush dab (eraser). Returns the touched region.
pub fn erase_dab(img: &mut ImageData, x: f32, y: f32, brush: &Brush) -> PxRect {
    let r = brush.bounds(x, y, img.width, img.height);
    for py in r.y..r.bottom() {
        for px in r.x..r.right() {
            let d = ((px as f32 - x).powi(2) + (py as f32 - y).powi(2)).sqrt();
            let c = brush.coverage(d);
            if c > 0.0 {
                let mut p = img.get(px, py);
                p[3] = (p[3] as f32 * (1.0 - c)).round() as u8;
                img.set(px, py, p);
            }
        }
    }
    r
}

impl TextureEditor {
    /// Mark the image as changed (re-upload + new revision).
    fn touch(&mut self) {
        self.dirty = true;
        self.revision += 1;
    }

    /// Changes whenever the image does (edits, undo/redo, open).
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Start editing `image` (clears undo history, cancels a running operation).
    pub fn open_image(&mut self, image: ImageData) {
        self.adjust.resize = [image.width, image.height];
        self.history = Some(ImageEditHistory::new(image));
        self.job = None;
        self.stroke_last = None;
        self.drag_start = None;
        self.touch();
        self.fit_pending = true;
        self.pan = egui::Vec2::ZERO;
    }

    /// Start a new transparent-white image.
    pub fn new_image(&mut self, width: u32, height: u32) {
        self.open_image(ImageData::filled(width.max(1), height.max(1), [255, 255, 255, 255]));
    }

    /// The image being edited.
    pub fn image(&self) -> Option<&ImageData> {
        self.history.as_ref().map(ImageEditHistory::image)
    }

    /// Undo history (for custom operations).
    pub fn history_mut(&mut self) -> Option<&mut ImageEditHistory> {
        self.touch();
        self.history.as_mut()
    }

    /// Apply a whole-image operation as one undo step, synchronously (small images, tests;
    /// the UI uses [`Self::apply_async`]).
    pub fn apply(&mut self, label: &str, op: impl FnOnce(&mut ImageData)) {
        if let Some(h) = &mut self.history {
            h.apply(label, op);
            self.touch();
        }
    }

    /// Replace the image with a new one (e.g. resize) as one undo step, synchronously.
    pub fn replace(&mut self, label: &str, op: impl FnOnce(&ImageData) -> ImageData) {
        self.apply(label, |img| *img = op(img));
    }

    /// Run a whole-image operation on a copy of the image on the compute pool; the result is
    /// committed as one undo step by [`Self::poll_io`] once done. Ignored while another
    /// operation runs (see [`Self::is_busy`]).
    pub fn apply_async(&mut self, label: &str, op: impl FnOnce(&mut ImageData) + Send + 'static) {
        self.replace_async(label, move |img| {
            let mut out = img.clone();
            op(&mut out);
            out
        });
    }

    /// Like [`Self::apply_async`] for operations producing a new image (resize, blur...).
    pub fn replace_async(&mut self, label: &str, op: impl FnOnce(&ImageData) -> ImageData + Send + 'static) {
        if self.job.is_some() {
            return;
        }
        let Some(img) = self.image().cloned() else { return };
        self.end_stroke();
        self.drag_start = None;
        self.status = format!("{label}...");
        self.job = Some(ImageJob {
            label: label.to_owned(),
            task: space3d_tasks::spawn_compute(move || op(&img)),
        });
    }

    /// Whether a background operation is running (editing is paused until it finishes).
    pub fn is_busy(&self) -> bool {
        self.job.is_some()
    }

    /// Commit a finished background operation.
    fn poll_job(&mut self) {
        let Some(job) = &mut self.job else { return };
        let result = match job.task.poll() {
            Some(img) => Ok(img),
            None if matches!(job.task.state(), space3d_tasks::TaskState::Cancelled) => Err(()),
            None => return,
        };
        let label = std::mem::take(&mut job.label);
        self.job = None;
        match result {
            Ok(img) => {
                let size = (img.width, img.height);
                self.apply(&label, move |i| *i = img);
                self.status = format!("{label} ({}x{})", size.0, size.1);
            }
            Err(()) => self.status = format!("{label} failed"),
        }
    }

    /// Undo the last edit.
    pub fn undo(&mut self) -> bool {
        if self.job.is_some() {
            return false;
        }
        let done = self.history.as_mut().is_some_and(ImageEditHistory::undo);
        if done {
            self.touch();
        }
        done
    }

    /// Redo the last undone edit.
    pub fn redo(&mut self) -> bool {
        if self.job.is_some() {
            return false;
        }
        let done = self.history.as_mut().is_some_and(ImageEditHistory::redo);
        if done {
            self.touch();
        }
        done
    }

    fn paint_brush(&self) -> Brush {
        Brush {
            color: self.color,
            ..self.brush
        }
    }

    /// Begin a brush/eraser stroke at pixel coordinates (one undo step per stroke).
    pub fn begin_stroke(&mut self, x: f32, y: f32) {
        if self.job.is_some() {
            return;
        }
        let brush = self.paint_brush();
        let tool = self.tool;
        let Some(h) = &mut self.history else { return };
        h.checkpoint(tool.label());
        let img = h.image_mut();
        if tool == TextureTool::Eraser {
            erase_dab(img, x, y, &brush);
        } else {
            tex::brush_dab(img, x, y, &brush);
        }
        self.stroke_last = Some((x, y));
        self.touch();
    }

    /// Continue the current stroke to `(x, y)`.
    pub fn continue_stroke(&mut self, x: f32, y: f32) {
        let brush = self.paint_brush();
        let tool = self.tool;
        let (Some(last), Some(h)) = (self.stroke_last, &mut self.history) else {
            return;
        };
        if (last.0 - x).abs() < 0.01 && (last.1 - y).abs() < 0.01 {
            return;
        }
        let img = h.image_mut();
        if tool == TextureTool::Eraser {
            let spacing = (brush.radius * 0.25).max(0.5);
            let d = ((x - last.0).powi(2) + (y - last.1).powi(2)).sqrt();
            let n = (d / spacing).ceil().max(1.0) as u32;
            for i in 1..=n {
                let t = i as f32 / n as f32;
                erase_dab(img, last.0 + (x - last.0) * t, last.1 + (y - last.1) * t, &brush);
            }
        } else {
            tex::brush_stroke(img, last, (x, y), &brush, (brush.radius * 0.25).max(0.5));
        }
        self.stroke_last = Some((x, y));
        self.touch();
    }

    /// End the current stroke.
    pub fn end_stroke(&mut self) {
        self.stroke_last = None;
    }

    /// Flood fill at a pixel.
    pub fn fill_at(&mut self, x: u32, y: u32) {
        let (color, tol) = (self.color, self.fill_tolerance);
        self.apply("Fill", |img| {
            if x < img.width && y < img.height {
                tex::flood_fill(img, x, y, color, tol);
            }
        });
    }

    /// Draw a line between pixels.
    pub fn line(&mut self, from: (i32, i32), to: (i32, i32)) {
        let color = self.color;
        self.apply("Line", |img| tex::draw_line(img, from, to, color));
    }

    /// Draw a rectangle spanning two corner pixels.
    pub fn rect(&mut self, a: (i32, i32), b: (i32, i32)) {
        let (color, filled) = (self.color, self.rect_filled);
        let (x, y) = (a.0.min(b.0), a.1.min(b.1));
        let (w, h) = ((a.0 - b.0).unsigned_abs() + 1, (a.1 - b.1).unsigned_abs() + 1);
        self.apply("Rect", |img| tex::draw_rect(img, x, y, w, h, color, filled));
    }

    /// Pick the color under a pixel.
    pub fn pick(&mut self, x: u32, y: u32) {
        if let Some(img) = self.image()
            && x < img.width
            && y < img.height
        {
            self.color = img.get(x, y);
        }
    }

    /// PNG bytes of the current image (synchronous; [`Self::save`] encodes in the background).
    pub fn encode_png(&self) -> Result<Vec<u8>, String> {
        let img = self.image().ok_or("no image")?;
        tex::encode_png(img).map_err(|e| e.to_string())
    }

    /// Status line (last operation / I/O result).
    pub fn status(&self) -> &str {
        &self.status
    }

    /// Encode the image as PNG and save it to [`Self::path`], both in the background.
    pub fn save(&mut self) {
        let Some(img) = self.image().cloned() else {
            self.status = "no image".into();
            return;
        };
        self.status = format!("saving {}...", self.path);
        self.writes.push(
            "save",
            write_file_with(self.path.clone(), move || {
                tex::encode_png(&img).map_err(|e| e.to_string())
            }),
        );
    }

    /// Read and decode [`Self::path`] in the background.
    pub fn load(&mut self) {
        self.status = format!("loading {}...", self.path);
        let task = read_file_with(self.path.clone(), |path, bytes| {
            let ext = std::path::Path::new(path).extension().and_then(|e| e.to_str());
            tex::decode_image_with_hint(&bytes, ext).map_err(|e| format!("{path}: {e}"))
        });
        self.reads.push(self.path.clone(), task);
    }

    /// Whether a file read/write is in flight.
    pub fn io_pending(&self) -> bool {
        !self.reads.is_empty() || !self.writes.is_empty()
    }

    /// Finish background I/O and operations (called every frame by the editor).
    pub fn poll_io(&mut self) {
        self.poll_job();
        for (path, result) in self.reads.poll() {
            match result {
                Ok(img) => {
                    self.status = format!("opened {path} ({}x{})", img.width, img.height);
                    self.open_image(img);
                }
                Err(e) => self.status = e,
            }
        }
        for (_, result) in self.writes.poll() {
            self.status = result.unwrap_or_else(|e| e);
        }
    }

    /// Largest texture side the display copy may have (from egui's `max_texture_side`,
    /// updated every frame by [`Self::show`]).
    pub fn set_max_texture_side(&mut self, max_side: usize) {
        if max_side != self.max_texture_side {
            self.max_texture_side = max_side.max(1);
            self.dirty = true;
        }
    }

    fn sync_texture(&mut self, ctx: &egui::Context) {
        if !self.dirty {
            return;
        }
        let Some(full) = self.history.as_ref().map(ImageEditHistory::image) else {
            return;
        };
        let img = display_image(full, self.max_texture_side);
        let color = egui::ColorImage::from_rgba_unmultiplied([img.width as usize, img.height as usize], &img.pixels);
        match &mut self.texture {
            Some(t) => t.set(color, egui::TextureOptions::NEAREST),
            None => {
                self.texture = Some(ctx.load_texture("space3d texture editor", color, egui::TextureOptions::NEAREST))
            }
        }
        self.dirty = false;
    }

    /// Show the editor window.
    pub fn show(&mut self, ctx: &egui::Context) {
        self.poll_io();
        if !self.open {
            return;
        }
        if self.history.is_none() {
            self.new_image(self.new_size[0], self.new_size[1]);
        }
        self.set_max_texture_side(ctx.input(|i| i.max_texture_side));
        self.sync_texture(ctx);
        let mut open = self.open;
        egui::Window::new("Texture editor")
            .id(egui::Id::new("space3d_texture_editor"))
            .open(&mut open)
            .default_size([760.0, 500.0])
            .show(ctx, |ui| self.contents(ui));
        self.open = open;
    }

    fn contents(&mut self, ui: &mut egui::Ui) {
        let busy = self.is_busy();
        ui.horizontal_wrapped(|ui| {
            ui.add_enabled_ui(!busy, |ui| self.toolbar(ui));
            if busy {
                ui.spinner();
            }
        });
        if !self.status.is_empty() {
            ui.label(egui::RichText::new(&self.status).weak());
        }
        ui.separator();
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(200.0);
                egui::ScrollArea::vertical().id_salt("tex_tools").show(ui, |ui| {
                    ui.add_enabled_ui(!busy, |ui| self.side_panel(ui));
                });
            });
            ui.separator();
            self.canvas(ui);
        });
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.add(egui::DragValue::new(&mut self.new_size[0]).range(1..=4096).prefix("w "));
            ui.add(egui::DragValue::new(&mut self.new_size[1]).range(1..=4096).prefix("h "));
            if ui.button("New").clicked() {
                self.new_image(self.new_size[0], self.new_size[1]);
            }
            ui.separator();
            ui.add(egui::TextEdit::singleline(&mut self.path).desired_width(140.0));
            if ui.button("Open").clicked() {
                self.load();
            }
            if ui.button("Save PNG").clicked() {
                self.save();
            }
            ui.separator();
            let (can_undo, can_redo) = self
                .history
                .as_ref()
                .map_or((false, false), |h| (h.can_undo(), h.can_redo()));
            if ui.add_enabled(can_undo, egui::Button::new("Undo")).clicked() {
                self.undo();
            }
            if ui.add_enabled(can_redo, egui::Button::new("Redo")).clicked() {
                self.redo();
            }
            ui.separator();
            if ui.button("Use on selection").clicked() {
                self.apply_to_selection = true;
            }
        });
    }

    fn side_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            for t in TextureTool::ALL {
                ui.selectable_value(&mut self.tool, t, t.label());
            }
        });
        property(ui, "Color", |ui| {
            ui.color_edit_button_srgba_unmultiplied(&mut self.color)
        });
        property(ui, "Radius", |ui| {
            fill_slider(ui, egui::Slider::new(&mut self.brush.radius, 0.5..=64.0))
        });
        property(ui, "Opacity", |ui| {
            fill_slider(ui, egui::Slider::new(&mut self.brush.opacity, 0.0..=1.0))
        });
        property(ui, "Hardness", |ui| {
            fill_slider(ui, egui::Slider::new(&mut self.brush.hardness, 0.0..=1.0))
        });
        property(ui, "Fill tol.", |ui| {
            fill_slider(ui, egui::Slider::new(&mut self.fill_tolerance, 0..=255))
        });
        property(ui, "Rect", |ui| ui.checkbox(&mut self.rect_filled, "filled"));
        property(ui, "Zoom", |ui| {
            let fit = ui.small_button("Fit").clicked();
            fill_slider(ui, egui::Slider::new(&mut self.zoom, 0.05..=32.0).logarithmic(true));
            if fit {
                self.fit_pending = true;
                self.pan = egui::Vec2::ZERO;
            }
        });
        let a = &mut self.adjust;
        let mut op: Option<EditorOp> = None;
        section(
            ui,
            "tex_color",
            "Color",
            true,
            |_| {},
            |ui| {
                property(ui, "Brightness", |ui| {
                    fill_slider(ui, egui::Slider::new(&mut a.brightness, -1.0..=1.0))
                });
                property(ui, "Contrast", |ui| {
                    fill_slider(ui, egui::Slider::new(&mut a.contrast, 0.0..=3.0))
                });
                if ui.button("Apply brightness/contrast").clicked() {
                    let (b, c) = (a.brightness, a.contrast);
                    op = Some(Box::new(move |e| {
                        e.apply_async("Brightness/contrast", move |i| tex::brightness_contrast(i, b, c))
                    }));
                }
                property(ui, "Hue", |ui| {
                    fill_slider(ui, egui::Slider::new(&mut a.hue, -180.0..=180.0))
                });
                property(ui, "Saturation", |ui| {
                    fill_slider(ui, egui::Slider::new(&mut a.saturation, 0.0..=3.0))
                });
                property(ui, "Lightness", |ui| {
                    fill_slider(ui, egui::Slider::new(&mut a.lightness, -1.0..=1.0))
                });
                ui.horizontal_wrapped(|ui| {
                    if ui.button("Apply hue/sat.").clicked() {
                        let (h, s, l) = (a.hue, a.saturation, a.lightness);
                        op = Some(Box::new(move |e| {
                            e.apply_async("Hue/saturation", move |i| tex::hue_saturation(i, h, s, l))
                        }));
                    }
                    if ui.button("Grayscale").clicked() {
                        op = Some(Box::new(|e| e.apply_async("Grayscale", tex::grayscale)));
                    }
                    if ui.button("Invert").clicked() {
                        op = Some(Box::new(|e| e.apply_async("Invert", tex::invert)));
                    }
                });
            },
        );
        section(
            ui,
            "tex_filter",
            "Filter / transform",
            true,
            |_| {},
            |ui| {
                property(ui, "Blur σ", |ui| {
                    let blur = ui.small_button("Blur").clicked();
                    fill_slider(ui, egui::Slider::new(&mut a.blur_sigma, 0.3..=16.0));
                    if blur {
                        let s = a.blur_sigma;
                        op = Some(Box::new(move |e| {
                            e.replace_async("Blur", move |i| tex::blur(i, Blur::Gaussian { sigma: s }))
                        }));
                    }
                });
                property(ui, "Resize", |ui| {
                    ui.add(egui::DragValue::new(&mut a.resize[0]).range(1..=8192));
                    ui.add(egui::DragValue::new(&mut a.resize[1]).range(1..=8192));
                    if ui.small_button("Apply").clicked() {
                        let [w, h] = a.resize;
                        op = Some(Box::new(move |e| {
                            e.replace_async("Resize", move |i| tex::resize(i, w, h, Filter::Bilinear))
                        }));
                    }
                });
                property(ui, "Flip", |ui| {
                    if ui.small_button("H").clicked() {
                        op = Some(Box::new(|e| e.apply_async("Flip horizontal", tex::flip_horizontal)));
                    }
                    if ui.small_button("V").clicked() {
                        op = Some(Box::new(|e| e.apply_async("Flip vertical", tex::flip_vertical)));
                    }
                });
                property(ui, "Rotate", |ui| {
                    if ui.small_button("90° ⟳").clicked() {
                        op = Some(Box::new(|e| e.replace_async("Rotate", tex::rotate90_cw)));
                    }
                    if ui.small_button("90° ⟲").clicked() {
                        op = Some(Box::new(|e| e.replace_async("Rotate", tex::rotate90_ccw)));
                    }
                    if ui.small_button("180°").clicked() {
                        op = Some(Box::new(|e| e.replace_async("Rotate", tex::rotate180)));
                    }
                });
            },
        );
        section(
            ui,
            "tex_generate",
            "Generate",
            false,
            |_| {},
            |ui| {
                property(ui, "Seed", |ui| fill_drag(ui, egui::DragValue::new(&mut a.noise_seed)));
                property(ui, "Scale", |ui| {
                    fill_slider(ui, egui::Slider::new(&mut a.noise_scale, 1.0..=256.0))
                });
                property(ui, "Octaves", |ui| {
                    fill_slider(ui, egui::Slider::new(&mut a.noise_octaves, 1..=8))
                });
                ui.horizontal_wrapped(|ui| {
                    if ui.button("Value noise").clicked() {
                        let (seed, scale, oct) = (a.noise_seed, a.noise_scale, a.noise_octaves);
                        op = Some(Box::new(move |e| {
                            e.replace_async("Noise", move |i| tex::value_noise(i.width, i.height, seed, scale, oct));
                        }));
                    }
                    if ui.button("Checker").clicked() {
                        op = Some(Box::new(|e| {
                            e.replace_async("Checker", |i| {
                                tex::checker(
                                    i.width,
                                    i.height,
                                    (i.width / 8).max(1),
                                    [230, 230, 230, 255],
                                    [60, 60, 60, 255],
                                )
                            });
                        }));
                    }
                });
                property(ui, "Strength", |ui| {
                    fill_slider(ui, egui::Slider::new(&mut a.normal_strength, 0.1..=10.0))
                });
                if ui.button("Normal map from height").clicked() {
                    let s = a.normal_strength;
                    op = Some(Box::new(move |e| {
                        e.replace_async("Normal map", move |i| tex::normal_map_from_height(i, s))
                    }));
                }
            },
        );
        if let Some(op) = op {
            op(self);
        }
    }

    fn canvas(&mut self, ui: &mut egui::Ui) {
        let Some((iw, ih)) = self.image().map(|i| (i.width as f32, i.height as f32)) else {
            return;
        };
        let avail = ui.available_size().max(egui::vec2(64.0, 64.0));
        let (rect, response) = ui.allocate_exact_size(avail, egui::Sense::click_and_drag());
        // Fit once the canvas has its real size (the first layout pass can be tiny).
        if self.fit_pending && rect.width() > 96.0 && rect.height() > 96.0 {
            self.fit_pending = false;
            self.zoom = ((rect.width() - 16.0) / iw)
                .min((rect.height() - 16.0) / ih)
                .clamp(0.05, 32.0);
        }
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, egui::Color32::from_gray(28));
        let size = egui::vec2(iw, ih) * self.zoom;
        let img_rect = egui::Rect::from_center_size(rect.center() + self.pan, size);

        // Checkerboard behind transparent pixels.
        let cell = 8.0;
        let vis = img_rect.intersect(rect);
        if vis.is_positive() {
            let (nx, ny) = ((vis.width() / cell).ceil() as i32, (vis.height() / cell).ceil() as i32);
            if nx * ny < 40_000 {
                for y in 0..ny {
                    for x in 0..nx {
                        if (x + y) % 2 == 0 {
                            let r = egui::Rect::from_min_size(
                                vis.min + egui::vec2(x as f32, y as f32) * cell,
                                egui::vec2(cell, cell),
                            )
                            .intersect(vis);
                            painter.rect_filled(r, 0.0, egui::Color32::from_gray(90));
                        }
                    }
                }
            }
        }
        if let Some(t) = &self.texture {
            painter.image(
                t.id(),
                img_rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        }
        painter.rect_stroke(
            img_rect,
            0.0,
            egui::Stroke::new(1.0, egui::Color32::from_gray(120)),
            egui::StrokeKind::Outside,
        );

        // Zoom around the cursor, pan with middle/secondary drag.
        let (scroll, pointer, primary_pressed, primary_down) = ui.input(|i| {
            (
                i.smooth_scroll_delta.y,
                i.pointer.hover_pos(),
                i.pointer.primary_pressed(),
                i.pointer.primary_down(),
            )
        });
        if response.hovered() && scroll != 0.0 {
            let old = self.zoom;
            self.zoom = (self.zoom * (scroll * 0.005).exp()).clamp(0.05, 32.0);
            if let Some(p) = pointer {
                let from_center = p - (rect.center() + self.pan);
                self.pan -= from_center * (self.zoom / old - 1.0);
            }
        }
        if response.dragged_by(egui::PointerButton::Middle) || response.dragged_by(egui::PointerButton::Secondary) {
            self.pan += response.drag_delta();
        }

        let to_px = |p: egui::Pos2| -> (f32, f32) {
            let local = (p - img_rect.min) / self.zoom;
            (local.x - 0.5, local.y - 0.5)
        };
        let to_screen = |x: f32, y: f32| img_rect.min + egui::vec2(x + 0.5, y + 0.5) * self.zoom;
        let Some(p) = pointer else {
            if !primary_down {
                self.end_stroke();
                self.drag_start = None;
            }
            return;
        };
        let (px, py) = to_px(p);
        let inside = px >= -0.5 && py >= -0.5 && px < iw - 0.5 && py < ih - 0.5;

        // Brush preview.
        if response.hovered() && matches!(self.tool, TextureTool::Brush | TextureTool::Eraser) {
            painter.circle_stroke(
                p,
                self.brush.radius * self.zoom,
                egui::Stroke::new(1.0, egui::Color32::WHITE),
            );
        }
        if self.is_busy() {
            return;
        }
        let start = response.hovered() && primary_pressed;
        match self.tool {
            TextureTool::Brush | TextureTool::Eraser => {
                if start {
                    self.begin_stroke(px, py);
                } else if self.stroke_last.is_some() && primary_down {
                    self.continue_stroke(px, py);
                } else if !primary_down {
                    self.end_stroke();
                }
            }
            TextureTool::Fill => {
                if start && inside {
                    self.fill_at(px.round().max(0.0) as u32, py.round().max(0.0) as u32);
                }
            }
            TextureTool::Picker => {
                if start && inside {
                    self.pick(px.round().max(0.0) as u32, py.round().max(0.0) as u32);
                }
            }
            TextureTool::Line | TextureTool::Rect => {
                if start {
                    self.drag_start = Some((px.round(), py.round()));
                }
                if let Some((sx, sy)) = self.drag_start {
                    let (ex, ey) = (px.round(), py.round());
                    let c = egui::Color32::from_rgba_unmultiplied(
                        self.color[0],
                        self.color[1],
                        self.color[2],
                        self.color[3],
                    );
                    if self.tool == TextureTool::Line {
                        painter.line_segment([to_screen(sx, sy), to_screen(ex, ey)], egui::Stroke::new(1.5, c));
                    } else {
                        painter.rect_stroke(
                            egui::Rect::from_two_pos(to_screen(sx, sy), to_screen(ex, ey)),
                            0.0,
                            egui::Stroke::new(1.5, c),
                            egui::StrokeKind::Middle,
                        );
                    }
                    if !primary_down {
                        let (a, b) = ((sx as i32, sy as i32), (ex as i32, ey as i32));
                        if self.tool == TextureTool::Line {
                            self.line(a, b);
                        } else {
                            self.rect(a, b);
                        }
                        self.drag_start = None;
                    }
                }
            }
        }
        if response.hovered() && inside {
            response.on_hover_text_at_pointer(format!("{:.0}, {:.0}", px.round(), py.round()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strokes_fill_shapes_and_undo() {
        let mut e = TextureEditor::default();
        e.new_image(32, 32);
        e.color = [255, 0, 0, 255];
        e.brush = Brush::hard([0; 4], 3.0);
        e.begin_stroke(5.0, 5.0);
        e.continue_stroke(20.0, 5.0);
        e.end_stroke();
        assert_eq!(e.image().unwrap().get(12, 5), [255, 0, 0, 255]);
        assert!(e.undo());
        assert_eq!(
            e.image().unwrap().get(12, 5),
            [255, 255, 255, 255],
            "whole stroke is one step"
        );
        assert!(e.redo());

        e.tool = TextureTool::Eraser;
        e.begin_stroke(12.0, 5.0);
        e.end_stroke();
        assert_eq!(e.image().unwrap().get(12, 5)[3], 0);

        e.color = [0, 0, 255, 255];
        e.fill_at(0, 31);
        assert_eq!(e.image().unwrap().get(31, 31), [0, 0, 255, 255]);
        e.color = [0, 255, 0, 255];
        e.rect((2, 20), (6, 24));
        assert_eq!(e.image().unwrap().get(2, 22), [0, 255, 0, 255]);
        e.line((0, 30), (10, 30));
        assert_eq!(e.image().unwrap().get(5, 30), [0, 255, 0, 255]);
        e.pick(31, 31);
        assert_eq!(e.color, [0, 0, 255, 255]);

        e.replace("Resize", |i| tex::resize(i, 8, 4, Filter::Nearest));
        assert_eq!((e.image().unwrap().width, e.image().unwrap().height), (8, 4));
        e.undo();
        assert_eq!(e.image().unwrap().width, 32);
        let png = e.encode_png().unwrap();
        assert_eq!(&png[1..4], b"PNG");
    }

    fn finish_job(e: &mut TextureEditor) {
        for _ in 0..2000 {
            e.poll_io();
            if !e.is_busy() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        panic!("background operation did not finish");
    }

    #[test]
    fn background_operations_commit_one_undo_step() {
        let mut e = TextureEditor::default();
        e.new_image(16, 8);
        let rev = e.revision();
        e.replace_async("Resize", |i| tex::resize(i, 4, 2, Filter::Nearest));
        assert!(e.is_busy());
        // Edits wait for the operation.
        e.begin_stroke(1.0, 1.0);
        e.end_stroke();
        assert!(!e.undo());
        e.replace_async("Ignored", |i| tex::resize(i, 1, 1, Filter::Nearest));
        finish_job(&mut e);
        assert_eq!((e.image().unwrap().width, e.image().unwrap().height), (4, 2));
        assert!(e.revision() > rev);
        e.apply_async("Invert", tex::invert);
        finish_job(&mut e);
        assert_eq!(e.image().unwrap().get(0, 0), [0, 0, 0, 255]);
        assert!(e.undo());
        assert!(e.undo());
        assert_eq!(e.image().unwrap().width, 16, "two operations, two undo steps");
        assert!(!e.undo());
    }

    #[test]
    fn display_copy_respects_the_texture_limit() {
        let img = ImageData::filled(300, 20, [1, 2, 3, 255]);
        assert!(matches!(display_image(&img, 300), Cow::Borrowed(_)));
        let small = display_image(&img, 64);
        assert_eq!((small.width, small.height), (64, 4));
        assert_eq!(small.get(10, 1), [1, 2, 3, 255]);
        let column = ImageData::filled(1, 5000, [0; 4]);
        let tall = display_image(&column, 100);
        assert_eq!((tall.width, tall.height), (1, 100));

        // The editor keeps the full image and uploads the downscaled copy.
        let ctx = egui::Context::default();
        let mut e = TextureEditor::default();
        e.open_image(img);
        e.set_max_texture_side(64);
        e.sync_texture(&ctx);
        assert_eq!(e.image().unwrap().width, 300);
        assert_eq!(e.texture.as_ref().unwrap().size(), [64, 4]);
    }
}
