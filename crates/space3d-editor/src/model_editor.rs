//! Model editor panel: a mesh edited with `space3d_assets::edit::mesh` operations, previewed
//! by an orbit camera rendering into an offscreen render target that is shown as an egui
//! image (`UiTextures::register_render_target`).
//!
//! Operations started from the UI run on the compute pool ([`ModelEditor::run_op_async`]) and
//! are committed as one undo step when done; model files are parsed and exports encoded in the
//! background too. Faces for extrusion are picked by clicking the preview in "select faces"
//! mode (highlighted in orange); without a selection, extrusion uses the faces facing
//! [`MeshOpParams::select_axis`].

use crate::components::EditorInternal;
use crate::files::{IoJobs, read_file_with, write_file_with};
use crate::widgets::{fill_slider, property, property_group, section, vec3_fields};
use space3d_assets::edit::mesh::{self as ops, MeshEditHistory, WeldOptions};
use space3d_core::glam::{EulerRot, Mat4, Quat, Vec3, Vec4};
use space3d_core::hecs::{Entity, World};
use space3d_core::{MeshData, Name, Resources, Transform};
use space3d_render::{
    Camera3d, Material, Mesh3d, MeshHandle, PointLight, Projection, RenderAssets, RenderTarget, TextureHandle,
};
use space3d_tasks::Task;
use space3d_ui::UiTextures;
use std::collections::BTreeSet;

/// Where the preview lives: far from the scene so scene cameras never see it.
pub const PREVIEW_ORIGIN: Vec3 = Vec3::new(0.0, -10_000.0, 0.0);
const PREVIEW_SIZE: (u32, u32) = (512, 384);
const PREVIEW_FOV: f32 = std::f32::consts::FRAC_PI_3;

/// Principal axis for selections and projections.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Axis {
    /// +X.
    PosX,
    /// -X.
    NegX,
    /// +Y.
    PosY,
    /// -Y.
    NegY,
    /// +Z.
    PosZ,
    /// -Z.
    NegZ,
}

impl Axis {
    const ALL: [Axis; 6] = [Axis::PosX, Axis::NegX, Axis::PosY, Axis::NegY, Axis::PosZ, Axis::NegZ];

    /// Unit vector.
    pub fn vec(self) -> Vec3 {
        match self {
            Axis::PosX => Vec3::X,
            Axis::NegX => Vec3::NEG_X,
            Axis::PosY => Vec3::Y,
            Axis::NegY => Vec3::NEG_Y,
            Axis::PosZ => Vec3::Z,
            Axis::NegZ => Vec3::NEG_Z,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Axis::PosX => "+X",
            Axis::NegX => "-X",
            Axis::PosY => "+Y",
            Axis::NegY => "-Y",
            Axis::PosZ => "+Z",
            Axis::NegZ => "-Z",
        }
    }
}

/// Parameters of the mesh operations.
#[derive(Clone, Debug, PartialEq)]
pub struct MeshOpParams {
    /// Translation.
    pub translate: Vec3,
    /// Euler rotation (degrees, XYZ).
    pub rotate_deg: Vec3,
    /// Scale factors.
    pub scale: Vec3,
    /// Weld distance.
    pub weld_epsilon: f32,
    /// Without an explicit face selection, extrusion uses the faces facing this axis.
    pub select_axis: Axis,
    /// Extrusion distance.
    pub extrude_distance: f32,
    /// UV projection scale.
    pub uv_scale: f32,
    /// Planar UV projection normal.
    pub uv_axis: Axis,
    /// Decimation grid resolution.
    pub decimate_resolution: u32,
    /// Vertex paint color (linear RGBA).
    pub paint_color: [f32; 4],
    /// Vertex paint radius (model units).
    pub paint_radius: f32,
    /// Vertex paint strength per dab.
    pub paint_strength: f32,
    /// Vertex paint hardness.
    pub paint_hardness: f32,
}

impl Default for MeshOpParams {
    fn default() -> Self {
        Self {
            translate: Vec3::ZERO,
            rotate_deg: Vec3::ZERO,
            scale: Vec3::ONE,
            weld_epsilon: 1e-4,
            select_axis: Axis::PosY,
            extrude_distance: 0.25,
            uv_scale: 1.0,
            uv_axis: Axis::PosY,
            decimate_resolution: 8,
            paint_color: [1.0, 0.2, 0.2, 1.0],
            paint_radius: 0.3,
            paint_strength: 0.5,
            paint_hardness: 0.5,
        }
    }
}

/// Result of a mesh operation: new mesh, undo label, status message.
type OpOutput = Result<(MeshData, &'static str, String), String>;

/// A mesh operation running in the background.
struct MeshJob {
    op: String,
    task: Task<OpOutput>,
}

/// Operations that keep triangle indices (so the face selection stays valid).
const PRESERVES_FACES: &[&str] = &[
    "transform",
    "center",
    "smooth_normals",
    "flat_normals",
    "flip_normals",
    "uv_box",
    "uv_planar",
    "fill_colors",
    "extrude",
];

/// Run operation `op` (see [`ModelEditor::run_op`]) on `mesh`; `faces` is the extrusion
/// selection (empty: faces facing `p.select_axis`).
fn compute_op(mut mesh: MeshData, op: &str, p: &MeshOpParams, faces: &[usize]) -> OpOutput {
    let (label, msg) = match op {
        "transform" => {
            let r = Quat::from_euler(
                EulerRot::XYZ,
                p.rotate_deg.x.to_radians(),
                p.rotate_deg.y.to_radians(),
                p.rotate_deg.z.to_radians(),
            );
            ops::transform(
                &mut mesh,
                Mat4::from_scale_rotation_translation(p.scale, r, p.translate),
            );
            ("Transform", "transformed".to_owned())
        }
        "center" => {
            let offset = ops::center_on_origin(&mut mesh);
            ("Center", format!("moved by {offset:.3}"))
        }
        "smooth_normals" => {
            ops::recalculate_smooth_normals(&mut mesh);
            ("Smooth normals", "smooth normals".to_owned())
        }
        "flat_normals" => {
            ops::recalculate_flat_normals(&mut mesh);
            ("Flat normals", "flat normals".to_owned())
        }
        "weld" => {
            let n = ops::weld_vertices(&mut mesh, &WeldOptions::positions_only(p.weld_epsilon));
            ("Weld", format!("welded {n} vertices"))
        }
        "subdivide" => {
            ops::subdivide(&mut mesh);
            ("Subdivide", "subdivided".to_owned())
        }
        "extrude" => {
            let faces = if faces.is_empty() {
                faces_facing(&mesh, p.select_axis.vec())
            } else {
                faces.to_vec()
            };
            if faces.is_empty() {
                return Err("no faces selected".into());
            }
            let added = ops::extrude_faces(&mut mesh, &faces, p.extrude_distance);
            (
                "Extrude",
                format!("extruded {} faces (+{} side triangles)", faces.len(), added.len()),
            )
        }
        "uv_box" => {
            ops::uv_box_projection(&mut mesh, p.uv_scale);
            ("UV box projection", "box-projected UVs".to_owned())
        }
        "uv_planar" => {
            ops::uv_planar_projection(&mut mesh, p.uv_axis.vec(), p.uv_scale);
            ("UV planar projection", "planar-projected UVs".to_owned())
        }
        "decimate" => {
            let n = ops::decimate_to_grid(&mut mesh, p.decimate_resolution);
            ("Decimate", format!("removed {n} triangles"))
        }
        "fill_colors" => {
            ops::fill_vertex_colors(&mut mesh, p.paint_color);
            ("Fill colors", "filled vertex colors".to_owned())
        }
        "flip_normals" => {
            ops::flip_normals(&mut mesh);
            ("Flip normals", "flipped normals".to_owned())
        }
        other => return Err(format!("unknown operation {other}")),
    };
    Ok((mesh, label, msg))
}

/// Parse an OBJ / glTF / GLB file into one merged mesh.
fn parse_model(path: &str, bytes: &[u8]) -> Result<MeshData, String> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let ctx = space3d_assets::LoadContext::new(path);
    let model = match ext.as_str() {
        "obj" => space3d_assets::loaders::load_obj(bytes, &ctx, false),
        "gltf" | "glb" => space3d_assets::loaders::load_gltf(bytes, &ctx, false),
        _ => return Err(format!("{path}: unsupported model format (obj, gltf, glb)")),
    }
    .map_err(|e| format!("{path}: {e}"))?;
    let mesh = model.merged_mesh();
    if mesh.positions.is_empty() {
        return Err(format!("{path}: no geometry"));
    }
    Ok(mesh)
}

struct Preview {
    target: TextureHandle,
    image: egui::TextureId,
    mesh: MeshHandle,
    model: Entity,
    camera: Entity,
    light: Entity,
}

/// Model editor state (resource inserted by `EditorPlugin`).
pub struct ModelEditor {
    /// Window shown.
    pub open: bool,
    /// Path for opening OBJ / glTF / GLB files.
    pub path: String,
    /// Path for exports (`.obj` or `.gltf`).
    pub export_path: String,
    /// Operation parameters.
    pub params: MeshOpParams,
    /// Orbit yaw (radians).
    pub yaw: f32,
    /// Orbit pitch (radians).
    pub pitch: f32,
    /// Orbit distance as a multiple of the model radius.
    pub zoom: f32,
    /// Preview color (linear RGBA).
    pub color: [f32; 4],
    /// Draw wireframe edges over the preview (meshes up to 4000 triangles).
    pub wireframe: bool,
    /// Drag on the preview paints vertex colors instead of orbiting.
    pub paint_mode: bool,
    /// Clicking the preview adds/removes the face under the pointer to/from the selection.
    pub select_mode: bool,
    /// Set when the user asked to add the mesh to the scene (consumed by the editor).
    pub add_to_scene: bool,
    history: MeshEditHistory,
    selected_faces: BTreeSet<usize>,
    job: Option<MeshJob>,
    preview: Option<Preview>,
    mesh_dirty: bool,
    painting: bool,
    status: String,
    reads: IoJobs<MeshData>,
    writes: IoJobs<String>,
}

impl Default for ModelEditor {
    fn default() -> Self {
        Self {
            open: false,
            path: "model.obj".into(),
            export_path: "model_export.obj".into(),
            params: MeshOpParams::default(),
            yaw: 0.6,
            pitch: 0.45,
            zoom: 2.6,
            color: [0.8, 0.8, 0.85, 1.0],
            wireframe: true,
            paint_mode: false,
            select_mode: false,
            add_to_scene: false,
            history: MeshEditHistory::new(MeshData::cube(1.0)),
            selected_faces: BTreeSet::new(),
            job: None,
            preview: None,
            mesh_dirty: true,
            painting: false,
            status: String::new(),
            reads: IoJobs::default(),
            writes: IoJobs::default(),
        }
    }
}

impl std::fmt::Debug for ModelEditor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let m = self.mesh();
        f.debug_struct("ModelEditor")
            .field("open", &self.open)
            .field("vertices", &m.vertex_count())
            .field("triangles", &m.triangle_count())
            .finish()
    }
}

/// Faces whose normal points along `axis` (dot > 0.7).
pub fn faces_facing(mesh: &MeshData, axis: Vec3) -> Vec<usize> {
    (0..mesh.triangle_count())
        .filter(|&t| {
            let i = &mesh.indices[t * 3..t * 3 + 3];
            let [a, b, c] = [0, 1, 2].map(|k| Vec3::from(mesh.positions[i[k] as usize]));
            (b - a).cross(c - a).normalize_or_zero().dot(axis) > 0.7
        })
        .collect()
}

/// Nearest ray/triangle hit distance (Möller–Trumbore), in model space.
pub fn raycast_mesh(mesh: &MeshData, origin: Vec3, dir: Vec3) -> Option<f32> {
    raycast_mesh_face(mesh, origin, dir).map(|(d, _)| d)
}

/// Nearest ray/triangle hit in model space: distance along `dir` and triangle index.
pub fn raycast_mesh_face(mesh: &MeshData, origin: Vec3, dir: Vec3) -> Option<(f32, usize)> {
    let mut best: Option<(f32, usize)> = None;
    for t in 0..mesh.triangle_count() {
        let i = &mesh.indices[t * 3..t * 3 + 3];
        let [a, b, c] = [0, 1, 2].map(|k| Vec3::from(mesh.positions[i[k] as usize]));
        let (e1, e2) = (b - a, c - a);
        let p = dir.cross(e2);
        let det = e1.dot(p);
        if det.abs() < 1e-9 {
            continue;
        }
        let inv = 1.0 / det;
        let s = origin - a;
        let u = s.dot(p) * inv;
        if !(0.0..=1.0).contains(&u) {
            continue;
        }
        let q = s.cross(e1);
        let v = dir.dot(q) * inv;
        if v < 0.0 || u + v > 1.0 {
            continue;
        }
        let d = e2.dot(q) * inv;
        if d > 1e-5 && best.is_none_or(|(b, _)| d < b) {
            best = Some((d, t));
        }
    }
    best
}

impl ModelEditor {
    /// The mesh being edited.
    pub fn mesh(&self) -> &MeshData {
        self.history.current()
    }

    /// Replace the mesh (clears undo history and the face selection, cancels a running
    /// operation).
    pub fn set_mesh(&mut self, mesh: MeshData) {
        self.history = MeshEditHistory::new(mesh);
        self.selected_faces.clear();
        self.job = None;
        self.painting = false;
        self.mesh_dirty = true;
    }

    /// Apply an operation as one undo step.
    pub fn apply<R>(&mut self, label: &str, op: impl FnOnce(&mut MeshData) -> R) -> R {
        self.mesh_dirty = true;
        self.history.apply(label, op)
    }

    fn after_history_step(&mut self, triangles_before: usize) {
        self.mesh_dirty = true;
        if self.mesh().triangle_count() != triangles_before {
            self.selected_faces.clear();
        }
    }

    /// Undo the last operation (not while a background operation runs).
    pub fn undo(&mut self) -> bool {
        let before = self.mesh().triangle_count();
        let done = self.job.is_none() && self.history.undo();
        if done {
            self.after_history_step(before);
        }
        done
    }

    /// Redo the last undone operation (not while a background operation runs).
    pub fn redo(&mut self) -> bool {
        let before = self.mesh().triangle_count();
        let done = self.job.is_none() && self.history.redo();
        if done {
            self.after_history_step(before);
        }
        done
    }

    /// Status line.
    pub fn status(&self) -> &str {
        &self.status
    }

    /// egui texture showing the preview render target, once created.
    pub fn preview_texture(&self) -> Option<egui::TextureId> {
        self.preview.as_ref().map(|p| p.image)
    }

    /// Faces (triangle indices) selected for extrusion.
    pub fn selected_faces(&self) -> &BTreeSet<usize> {
        &self.selected_faces
    }

    /// Add `face` to the selection, or remove it if selected. Returns whether it is selected.
    pub fn toggle_face(&mut self, face: usize) -> bool {
        if face >= self.mesh().triangle_count() {
            return false;
        }
        if self.selected_faces.remove(&face) {
            false
        } else {
            self.selected_faces.insert(face)
        }
    }

    /// Select the faces facing `axis` (replacing the selection).
    pub fn select_facing(&mut self, axis: Axis) {
        self.selected_faces = faces_facing(self.mesh(), axis.vec()).into_iter().collect();
    }

    /// Clear the face selection.
    pub fn clear_selection(&mut self) {
        self.selected_faces.clear();
    }

    /// Face under a point of the preview image (`uv` in 0..1, y down).
    pub fn pick_face(&self, uv: [f32; 2]) -> Option<usize> {
        let (eye, dir) = self.preview_ray(uv);
        raycast_mesh_face(self.mesh(), eye, dir).map(|(_, f)| f)
    }

    /// Whether a background operation is running (editing waits for it).
    pub fn is_busy(&self) -> bool {
        self.job.is_some()
    }

    /// Whether a file read/write is in flight.
    pub fn io_pending(&self) -> bool {
        !self.reads.is_empty() || !self.writes.is_empty()
    }

    fn commit(&mut self, op: &str, output: OpOutput) -> Result<String, String> {
        let (mesh, label, msg) = output?;
        self.history.apply(label, move |m| *m = mesh);
        self.mesh_dirty = true;
        if !PRESERVES_FACES.contains(&op) {
            self.selected_faces.clear();
        }
        let n = self.mesh().triangle_count();
        self.selected_faces.retain(|&f| f < n);
        self.status.clone_from(&msg);
        Ok(msg)
    }

    fn op_inputs(&self) -> (MeshData, MeshOpParams, Vec<usize>) {
        (
            self.mesh().clone(),
            self.params.clone(),
            self.selected_faces.iter().copied().collect(),
        )
    }

    /// Run the operation named `op` with the current [`MeshOpParams`] synchronously (as one
    /// undo step). Names: `transform`, `center`, `smooth_normals`, `flat_normals`, `weld`,
    /// `subdivide`, `extrude` (selected faces, else faces facing `select_axis`), `uv_box`,
    /// `uv_planar`, `decimate`, `fill_colors`, `flip_normals`. The UI uses
    /// [`Self::run_op_async`].
    pub fn run_op(&mut self, op: &str) -> Result<String, String> {
        if self.job.is_some() {
            return Err("another operation is running".into());
        }
        let (mesh, params, faces) = self.op_inputs();
        self.commit(op, compute_op(mesh, op, &params, &faces))
    }

    /// Start [`Self::run_op`]'s operation on the compute pool; [`Self::poll_io`] commits it as
    /// one undo step when done. Ignored while another operation runs.
    pub fn run_op_async(&mut self, op: &str) {
        if self.job.is_some() {
            return;
        }
        self.end_paint();
        let (mesh, params, faces) = self.op_inputs();
        let name = op.to_owned();
        self.status = format!("{op}...");
        self.job = Some(MeshJob {
            op: op.to_owned(),
            task: space3d_tasks::spawn_compute(move || compute_op(mesh, &name, &params, &faces)),
        });
    }

    fn poll_job(&mut self) {
        let Some(job) = &mut self.job else { return };
        let output = match job.task.poll() {
            Some(out) => out,
            None if matches!(job.task.state(), space3d_tasks::TaskState::Cancelled) => {
                Err("operation cancelled".into())
            }
            None => return,
        };
        let op = std::mem::take(&mut job.op);
        self.job = None;
        if let Err(e) = self.commit(&op, output) {
            self.status = e;
        }
    }

    /// Paint vertex colors at a model-space point (part of the current stroke if one is open).
    pub fn paint_at(&mut self, point: Vec3) -> usize {
        if self.job.is_some() {
            return 0;
        }
        let p = self.params.clone();
        self.mesh_dirty = true;
        if !self.painting {
            self.history.checkpoint("Vertex paint");
            self.painting = true;
        }
        ops::paint_vertex_colors(
            self.history.current_mut(),
            point,
            p.paint_radius,
            p.paint_color,
            p.paint_strength,
            p.paint_hardness,
        )
    }

    /// Close the current paint stroke.
    pub fn end_paint(&mut self) {
        self.painting = false;
    }

    /// Model-space eye position and view-projection of the orbit camera.
    fn camera(&self) -> (Vec3, Vec3, Mat4) {
        let (center, radius) = match ops::bounding_sphere(self.mesh()) {
            Some(s) => (s.center, s.radius.max(0.01)),
            None => (Vec3::ZERO, 1.0),
        };
        let dir = Vec3::new(
            self.pitch.cos() * self.yaw.sin(),
            self.pitch.sin(),
            self.pitch.cos() * self.yaw.cos(),
        );
        let eye = center + dir * radius * self.zoom;
        let view = space3d_core::glam::camera::rh::view::look_at_mat4(eye, center, Vec3::Y);
        let aspect = PREVIEW_SIZE.0 as f32 / PREVIEW_SIZE.1 as f32;
        let proj = Projection::Perspective {
            fov_y: PREVIEW_FOV,
            near: 0.01 * radius,
            far: 100.0 * radius * self.zoom.max(1.0),
        }
        .matrix(aspect);
        (eye, center, proj * view)
    }

    /// Model-space ray through a point of the preview image (`uv` in 0..1, y down).
    fn preview_ray(&self, uv: [f32; 2]) -> (Vec3, Vec3) {
        let (eye, _, view_proj) = self.camera();
        let ndc = Vec3::new(uv[0] * 2.0 - 1.0, 1.0 - uv[1] * 2.0, 1.0);
        let far = view_proj.inverse().project_point3(ndc);
        (eye, (far - eye).normalize_or_zero())
    }

    fn opened(&mut self, path: &str, mesh: MeshData) {
        self.status = format!(
            "opened {path}: {} vertices, {} triangles",
            mesh.vertex_count(),
            mesh.triangle_count()
        );
        self.set_mesh(mesh);
    }

    /// Parse model file bytes synchronously (the UI uses [`Self::open`]).
    #[cfg(test)]
    fn open_file(&mut self, path: &str, bytes: &[u8]) -> Result<(), String> {
        let mesh = parse_model(path, bytes)?;
        self.opened(path, mesh);
        Ok(())
    }

    /// Read and parse [`Self::path`] (OBJ, glTF or GLB) in the background.
    pub fn open(&mut self) {
        self.status = format!("loading {}...", self.path);
        let task = read_file_with(self.path.clone(), |path, bytes| parse_model(path, &bytes));
        self.reads.push(self.path.clone(), task);
    }

    /// Export to [`Self::export_path`] (`.gltf` → glTF JSON, anything else → OBJ), encoding and
    /// writing in the background.
    pub fn export(&mut self) {
        let mesh = self.mesh().clone();
        let gltf = self.export_path.to_ascii_lowercase().ends_with(".gltf");
        self.status = format!("exporting {}...", self.export_path);
        let task = write_file_with(self.export_path.clone(), move || {
            let text = if gltf { ops::to_gltf(&mesh) } else { ops::to_obj(&mesh) };
            Ok(text.into_bytes())
        });
        self.writes.push("export", task);
    }

    /// Finish background operations and I/O (called every frame by the editor).
    pub fn poll_io(&mut self) {
        self.poll_job();
        for (path, result) in self.reads.poll() {
            match result {
                Ok(mesh) => self.opened(&path, mesh),
                Err(e) => self.status = e,
            }
        }
        for (_, result) in self.writes.poll() {
            self.status = result.unwrap_or_else(|e| e);
        }
    }

    /// Create/update the preview entities and render target (called by the editor each frame).
    pub(crate) fn sync_preview(&mut self, world: &mut World, resources: &mut Resources, ctx: &egui::Context) {
        if self.preview.is_none() {
            if !self.open {
                return;
            }
            let Some(assets) = resources.get_mut::<RenderAssets>() else {
                return;
            };
            let target = assets.add_render_target(PREVIEW_SIZE.0, PREVIEW_SIZE.1);
            let mesh = assets.add_mesh(self.mesh().clone());
            let image = resources.init::<UiTextures>().register_render_target(ctx, target);
            let model = world.spawn((
                EditorInternal,
                Name("model editor preview".into()),
                Transform::from_xyz(PREVIEW_ORIGIN.x, PREVIEW_ORIGIN.y, PREVIEW_ORIGIN.z),
                Mesh3d(mesh),
                Material::color(Vec4::from(self.color)),
            ));
            let camera = world.spawn((
                EditorInternal,
                Transform::IDENTITY,
                Camera3d {
                    projection: Projection::Perspective {
                        fov_y: PREVIEW_FOV,
                        near: 0.01,
                        far: 100.0,
                    },
                    clear_color: Some(Vec4::new(0.12, 0.12, 0.14, 1.0)),
                    target: RenderTarget::Texture(target),
                    order: -100,
                    viewport: None,
                    active: true,
                },
            ));
            let light = world.spawn((
                EditorInternal,
                Transform::IDENTITY,
                PointLight {
                    color: Vec3::ONE,
                    intensity: 3.0,
                    range: 50.0,
                },
            ));
            self.preview = Some(Preview {
                target,
                image,
                mesh,
                model,
                camera,
                light,
            });
            self.mesh_dirty = false;
        }
        let Some(p) = &self.preview else { return };
        if self.mesh_dirty {
            if let Some(assets) = resources.get_mut::<RenderAssets>() {
                assets.update_mesh(p.mesh, self.mesh().clone());
            }
            self.mesh_dirty = false;
        }
        let (eye, center, _) = self.camera();
        let near_far = {
            let radius = ops::bounding_sphere(self.mesh()).map_or(1.0, |s| s.radius.max(0.01));
            (0.01 * radius, 100.0 * radius * self.zoom.max(1.0))
        };
        if let Ok(mut cam) = world.get::<&mut Camera3d>(p.camera) {
            cam.active = self.open;
            cam.projection = Projection::Perspective {
                fov_y: PREVIEW_FOV,
                near: near_far.0,
                far: near_far.1,
            };
        }
        if let Ok(mut t) = world.get::<&mut Transform>(p.camera) {
            *t = Transform::from_xyz(0.0, 0.0, 0.0).looking_at(center - eye, Vec3::Y);
            t.translation = PREVIEW_ORIGIN + eye;
        }
        if let Ok(mut t) = world.get::<&mut Transform>(p.light) {
            t.translation = PREVIEW_ORIGIN + eye + Vec3::Y * 0.5;
        }
        if let Ok(mut m) = world.get::<&mut Material>(p.model) {
            m.base_color = Vec4::from(self.color);
        }
    }

    /// Remove the preview entities and target.
    pub(crate) fn destroy_preview(&mut self, world: &mut World, resources: &mut Resources, ctx: &egui::Context) {
        let Some(p) = self.preview.take() else { return };
        for e in [p.model, p.camera, p.light] {
            let _ = world.despawn(e);
        }
        if let Some(assets) = resources.get_mut::<RenderAssets>() {
            assets.remove_mesh(p.mesh);
            assets.remove_texture(p.target);
        }
        if let Some(t) = resources.get_mut::<UiTextures>() {
            t.unregister(ctx, p.image);
        }
        self.mesh_dirty = true;
    }

    /// Show the editor window.
    pub fn show(&mut self, ctx: &egui::Context) {
        self.show_window(ctx, None);
    }

    /// Show the editor window within `area` (screen points; e.g. the viewport left between an
    /// editor's panels): it opens at its top-left corner and never leaves it.
    pub fn show_in(&mut self, ctx: &egui::Context, area: egui::Rect) {
        self.show_window(ctx, Some(area));
    }

    fn show_window(&mut self, ctx: &egui::Context, area: Option<egui::Rect>) {
        self.poll_io();
        if !self.open {
            return;
        }
        let mut open = self.open;
        let mut window = egui::Window::new("Model editor")
            .id(egui::Id::new("space3d_model_editor"))
            .open(&mut open)
            .default_size([820.0, 480.0]);
        if let Some(area) = area {
            window = window.default_pos(area.min + egui::vec2(8.0, 8.0)).constrain_to(area);
        }
        window.show(ctx, |ui| self.contents(ui));
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
        let m = self.mesh();
        let yes_no = |n: usize| if n == m.positions.len() { "✔" } else { "–" };
        let info = format!(
            "{} verts · {} tris · normals {} · uvs {} · colors {} · {} faces selected",
            m.vertex_count(),
            m.triangle_count(),
            yes_no(m.normals.len()),
            yes_no(m.uvs.len()),
            yes_no(m.colors.len()),
            self.selected_faces.len(),
        );
        ui.horizontal(|ui| {
            ui.label(info);
            if !self.status.is_empty() {
                ui.separator();
                ui.label(egui::RichText::new(&self.status).weak());
            }
        });
        ui.separator();
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(crate::widgets::tool_column_width(ui, 230.0));
                crate::widgets::visible_scroll_bars(ui);
                egui::ScrollArea::vertical().id_salt("model_ops").show(ui, |ui| {
                    ui.add_enabled_ui(!busy, |ui| self.ops_panel(ui));
                });
            });
            ui.separator();
            self.viewport(ui);
        });
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            if ui.button("Cube").clicked() {
                self.set_mesh(MeshData::cube(1.0));
            }
            if ui.button("Sphere").clicked() {
                self.set_mesh(MeshData::sphere(0.5, 24, 16));
            }
            if ui.button("Plane").clicked() {
                self.set_mesh(MeshData::plane(1.0));
            }
            ui.separator();
            ui.add(egui::TextEdit::singleline(&mut self.path).desired_width(120.0));
            if ui.button("Open").clicked() {
                self.open();
            }
            ui.separator();
            ui.add(egui::TextEdit::singleline(&mut self.export_path).desired_width(120.0));
            if ui.button("Export").clicked() {
                self.export();
            }
            ui.separator();
            if ui
                .add_enabled(self.history.can_undo(), egui::Button::new("Undo"))
                .clicked()
            {
                self.undo();
            }
            if ui
                .add_enabled(self.history.can_redo(), egui::Button::new("Redo"))
                .clicked()
            {
                self.redo();
            }
            if ui.button("Add to scene").clicked() {
                self.add_to_scene = true;
            }
        });
    }

    fn ops_panel(&mut self, ui: &mut egui::Ui) {
        let mut run: Option<&'static str> = None;
        // Some(Some(axis)): select faces facing axis; Some(None): clear the selection.
        let mut select: Option<Option<Axis>> = None;
        let selected = self.selected_faces.len();
        let p = &mut self.params;
        section(
            ui,
            "model_transform",
            "Transform",
            true,
            |_| {},
            |ui| {
                property(ui, "Move", |ui| vec3_fields(ui, &mut p.translate, 0.01));
                property(ui, "Rotate °", |ui| vec3_fields(ui, &mut p.rotate_deg, 1.0));
                property(ui, "Scale", |ui| vec3_fields(ui, &mut p.scale, 0.01));
                ui.horizontal(|ui| {
                    if ui.button("Apply").clicked() {
                        run = Some("transform");
                    }
                    if ui.button("Center").clicked() {
                        run = Some("center");
                    }
                });
            },
        );
        section(
            ui,
            "model_topology",
            "Topology",
            true,
            |_| {},
            |ui| {
                property_group(ui, "Normals", |ui| {
                    if ui.button("Smooth").clicked() {
                        run = Some("smooth_normals");
                    }
                    if ui.button("Flat").clicked() {
                        run = Some("flat_normals");
                    }
                    if ui.button("Flip").clicked() {
                        run = Some("flip_normals");
                    }
                });
                property_group(ui, "Weld ε", |ui| {
                    ui.add(egui::DragValue::new(&mut p.weld_epsilon).speed(0.0001).range(0.0..=1.0));
                    if ui.button("Weld").clicked() {
                        run = Some("weld");
                    }
                });
                property(ui, "Subdivide", |ui| {
                    if ui.button("Subdivide").clicked() {
                        run = Some("subdivide");
                    }
                });
                if ui
                    .checkbox(&mut self.select_mode, "select faces (click preview)")
                    .changed()
                    && self.select_mode
                {
                    self.paint_mode = false;
                }
                property_group(ui, "Facing", |ui| {
                    egui::ComboBox::from_id_salt("extrude_axis")
                        .selected_text(p.select_axis.label())
                        .width(44.0)
                        .show_ui(ui, |ui| {
                            for a in Axis::ALL {
                                ui.selectable_value(&mut p.select_axis, a, a.label());
                            }
                        });
                    if ui.button("Select").clicked() {
                        select = Some(Some(p.select_axis));
                    }
                    if ui.add_enabled(selected > 0, egui::Button::new("Clear")).clicked() {
                        select = Some(None);
                    }
                });
                property_group(ui, "Extrude", |ui| {
                    ui.add(egui::DragValue::new(&mut p.extrude_distance).speed(0.01));
                    let hint = if selected == 0 {
                        "No faces selected: extrudes the faces facing the axis".to_owned()
                    } else {
                        format!("Extrudes the {selected} selected faces")
                    };
                    if ui.button("Extrude").on_hover_text(hint).clicked() {
                        run = Some("extrude");
                    }
                });
                property_group(ui, "Decimate", |ui| {
                    ui.add(egui::DragValue::new(&mut p.decimate_resolution).range(1..=256));
                    if ui.button("Decimate").clicked() {
                        run = Some("decimate");
                    }
                });
            },
        );
        section(
            ui,
            "model_uvs",
            "UVs",
            false,
            |_| {},
            |ui| {
                property(ui, "Scale", |ui| {
                    fill_slider(ui, egui::Slider::new(&mut p.uv_scale, 0.01..=10.0).logarithmic(true))
                });
                property(ui, "Box", |ui| {
                    if ui.button("Project").clicked() {
                        run = Some("uv_box");
                    }
                });
                property_group(ui, "Planar", |ui| {
                    egui::ComboBox::from_id_salt("uv_axis")
                        .selected_text(p.uv_axis.label())
                        .width(44.0)
                        .show_ui(ui, |ui| {
                            for a in Axis::ALL {
                                ui.selectable_value(&mut p.uv_axis, a, a.label());
                            }
                        });
                    if ui.button("Project").clicked() {
                        run = Some("uv_planar");
                    }
                });
            },
        );
        section(
            ui,
            "model_paint",
            "Vertex paint",
            false,
            |_| {},
            |ui| {
                if ui.checkbox(&mut self.paint_mode, "paint on drag").changed() && self.paint_mode {
                    self.select_mode = false;
                }
                property_group(ui, "Color", |ui| {
                    let mut c = egui::Rgba::from_rgba_unmultiplied(
                        p.paint_color[0],
                        p.paint_color[1],
                        p.paint_color[2],
                        p.paint_color[3],
                    );
                    if egui::color_picker::color_edit_button_rgba(ui, &mut c, egui::color_picker::Alpha::OnlyBlend)
                        .changed()
                    {
                        p.paint_color = c.to_rgba_unmultiplied();
                    }
                    if ui.button("Fill all").clicked() {
                        run = Some("fill_colors");
                    }
                });
                property(ui, "Radius", |ui| {
                    fill_slider(ui, egui::Slider::new(&mut p.paint_radius, 0.01..=5.0).logarithmic(true))
                });
                property(ui, "Strength", |ui| {
                    fill_slider(ui, egui::Slider::new(&mut p.paint_strength, 0.0..=1.0))
                });
                property(ui, "Hardness", |ui| {
                    fill_slider(ui, egui::Slider::new(&mut p.paint_hardness, 0.0..=1.0))
                });
            },
        );
        section(
            ui,
            "model_preview",
            "Preview",
            false,
            |_| {},
            |ui| {
                ui.checkbox(&mut self.wireframe, "wireframe overlay");
                property(ui, "Material", |ui| {
                    let mut c =
                        egui::Rgba::from_rgba_unmultiplied(self.color[0], self.color[1], self.color[2], self.color[3]);
                    if egui::color_picker::color_edit_button_rgba(ui, &mut c, egui::color_picker::Alpha::OnlyBlend)
                        .changed()
                    {
                        self.color = c.to_rgba_unmultiplied();
                    }
                });
            },
        );
        match select {
            Some(Some(axis)) => self.select_facing(axis),
            Some(None) => self.clear_selection(),
            None => {}
        }
        if let Some(op) = run {
            self.run_op_async(op);
        }
    }

    fn viewport(&mut self, ui: &mut egui::Ui) {
        let avail = ui.available_size().max(egui::vec2(120.0, 90.0));
        let aspect = PREVIEW_SIZE.0 as f32 / PREVIEW_SIZE.1 as f32;
        let size = if avail.x / avail.y > aspect {
            egui::vec2(avail.y * aspect, avail.y)
        } else {
            egui::vec2(avail.x, avail.x / aspect)
        };
        // Centered in the space left of the window (no gutter on one side only).
        ui.add_space(((ui.available_width() - size.x) / 2.0).floor().max(0.0));
        let response = match self.preview_texture() {
            Some(id) => {
                ui.add(egui::Image::new(egui::load::SizedTexture::new(id, size)).sense(egui::Sense::click_and_drag()))
            }
            None => {
                let (rect, r) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
                ui.painter().rect_filled(rect, 0.0, egui::Color32::from_gray(30));
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "no renderer",
                    egui::FontId::proportional(14.0),
                    egui::Color32::GRAY,
                );
                r
            }
        };
        let rect = response.rect;
        let to_uv = |pos: egui::Pos2| {
            [
                (pos.x - rect.left()) / rect.width().max(1.0),
                (pos.y - rect.top()) / rect.height().max(1.0),
            ]
        };

        // Orbit / zoom / paint / select.
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if response.hovered() && scroll != 0.0 {
            self.zoom = (self.zoom * (-scroll * 0.004).exp()).clamp(0.3, 50.0);
        }
        let busy = self.is_busy();
        if self.select_mode
            && !busy
            && response.clicked()
            && let Some(pos) = response.interact_pointer_pos()
            && let Some(face) = self.pick_face(to_uv(pos))
        {
            let on = self.toggle_face(face);
            self.status = format!(
                "face {face} {} · {} selected",
                if on { "selected" } else { "deselected" },
                self.selected_faces.len()
            );
        }
        let paint = self.paint_mode && !busy && response.dragged_by(egui::PointerButton::Primary);
        if paint {
            if let Some(pos) = response.interact_pointer_pos() {
                let (eye, dir) = self.preview_ray(to_uv(pos));
                if let Some(t) = raycast_mesh(self.mesh(), eye, dir) {
                    self.paint_at(eye + dir * t);
                }
            }
        } else {
            if self.painting {
                self.end_paint();
            }
            if response.dragged() {
                let d = response.drag_delta();
                self.yaw -= d.x * 0.01;
                self.pitch = (self.pitch + d.y * 0.01).clamp(-1.5, 1.5);
            }
        }

        // Wireframe overlay and selected faces.
        let (eye, _, view_proj) = self.camera();
        let mesh = self.history.current();
        let painter = ui.painter_at(rect);
        let project = |v: [f32; 3]| -> Option<egui::Pos2> {
            let clip = view_proj * Vec3::from(v).extend(1.0);
            if clip.w <= 1e-4 {
                return None;
            }
            let n = clip.truncate() / clip.w;
            Some(egui::pos2(
                rect.left() + (n.x * 0.5 + 0.5) * rect.width(),
                rect.top() + (0.5 - n.y * 0.5) * rect.height(),
            ))
        };
        if !self.selected_faces.is_empty() {
            let front = egui::Color32::from_rgba_unmultiplied(255, 140, 0, 150);
            let back = egui::Color32::from_rgba_unmultiplied(255, 140, 0, 40);
            let edge = egui::Stroke::new(1.0, egui::Color32::from_rgb(255, 170, 60));
            for &f in self.selected_faces.iter().take(20_000) {
                let Some(tri) = mesh.indices.get(f * 3..f * 3 + 3) else {
                    continue;
                };
                let [a, b, c] = [0, 1, 2].map(|k| mesh.positions[tri[k] as usize]);
                let (Some(pa), Some(pb), Some(pc)) = (project(a), project(b), project(c)) else {
                    continue;
                };
                let (a, b, c) = (Vec3::from(a), Vec3::from(b), Vec3::from(c));
                let facing = (b - a).cross(c - a).dot(eye - a) > 0.0;
                painter.add(egui::Shape::convex_polygon(
                    vec![pa, pb, pc],
                    if facing { front } else { back },
                    edge,
                ));
            }
        }
        if self.wireframe && mesh.triangle_count() <= 4000 {
            let stroke = egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(255, 255, 255, 60));
            for tri in mesh.indices.as_chunks::<3>().0 {
                let pts = tri.map(|i| project(mesh.positions[i as usize]));
                for k in 0..3 {
                    if let (Some(a), Some(b)) = (pts[k], pts[(k + 1) % 3]) {
                        painter.line_segment([a, b], stroke);
                    }
                }
            }
        }
        response.on_hover_text(if self.select_mode {
            "click: select/deselect face · drag: orbit · scroll: zoom"
        } else {
            "drag: orbit (or paint) · scroll: zoom"
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ops_undo_and_selection() {
        let mut e = ModelEditor::default();
        assert_eq!(e.mesh().triangle_count(), 12);
        e.run_op("subdivide").unwrap();
        assert_eq!(e.mesh().triangle_count(), 48);
        let top = faces_facing(e.mesh(), Vec3::Y).len();
        assert_eq!(top, 8);
        e.run_op("extrude").unwrap();
        assert!(e.mesh().triangle_count() > 48);
        e.params.scale = Vec3::splat(2.0);
        e.run_op("transform").unwrap();
        e.run_op("weld").unwrap();
        e.run_op("uv_box").unwrap();
        e.run_op("decimate").unwrap();
        e.run_op("smooth_normals").unwrap();
        assert!(e.run_op("nope").is_err());
        for _ in 0..7 {
            assert!(e.undo());
        }
        assert_eq!(e.mesh().triangle_count(), 12);
        assert!(e.redo());
        assert_eq!(e.mesh().triangle_count(), 48);
    }

    #[test]
    fn paint_stroke_is_one_step() {
        let mut e = ModelEditor::default();
        let hit = raycast_mesh(e.mesh(), Vec3::new(0.0, 0.0, 5.0), Vec3::NEG_Z).expect("hits the cube");
        assert!((hit - 4.5).abs() < 1e-4);
        assert!(raycast_mesh(e.mesh(), Vec3::new(5.0, 5.0, 5.0), Vec3::Z).is_none());
        e.params.paint_color = [1.0, 0.0, 0.0, 1.0];
        e.params.paint_strength = 1.0;
        e.params.paint_radius = 0.8;
        assert!(e.paint_at(Vec3::new(0.5, 0.5, 0.5)) > 0);
        e.paint_at(Vec3::new(-0.5, 0.5, 0.5));
        e.end_paint();
        assert_eq!(e.mesh().colors.len(), e.mesh().positions.len());
        e.undo();
        assert!(e.mesh().colors.is_empty(), "stroke undone in one step");
    }

    #[test]
    fn face_selection_drives_extrude() {
        let mut e = ModelEditor::default();
        let (_, front) = raycast_mesh_face(e.mesh(), Vec3::new(0.2, 0.1, 5.0), Vec3::NEG_Z).unwrap();
        assert!(faces_facing(e.mesh(), Vec3::Z).contains(&front));
        assert!(e.toggle_face(front));
        assert!(!e.toggle_face(front), "second click deselects");
        assert!(!e.toggle_face(999), "out of range");
        e.toggle_face(front);
        // One triangle: 3 boundary edges → 6 side triangles; the cap keeps its index.
        e.run_op("extrude").unwrap();
        assert_eq!(e.mesh().triangle_count(), 12 + 6);
        assert_eq!(e.selected_faces().iter().copied().collect::<Vec<_>>(), vec![front]);
        // Topology changes clear the selection; undo past a triangle-count change too.
        e.run_op("subdivide").unwrap();
        assert!(e.selected_faces().is_empty());
        e.select_facing(Axis::PosY);
        let up = faces_facing(e.mesh(), Vec3::Y).len();
        assert!(up >= 8);
        assert_eq!(e.selected_faces().len(), up);
        assert!(e.undo());
        assert!(e.selected_faces().is_empty());
        // Picking through the preview camera hits the model.
        assert!(e.pick_face([0.5, 0.5]).is_some());
        assert_eq!(e.pick_face([0.0, 0.0]), None);
    }

    fn settle(e: &mut ModelEditor) {
        for _ in 0..4000 {
            e.poll_io();
            if !e.is_busy() && !e.io_pending() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        panic!("background work did not finish: {}", e.status());
    }

    #[test]
    fn background_ops_commit_one_undo_step() {
        let mut e = ModelEditor::default();
        e.run_op_async("subdivide");
        assert!(e.is_busy());
        assert!(e.run_op("weld").is_err(), "one operation at a time");
        assert!(!e.undo());
        settle(&mut e);
        assert_eq!(e.mesh().triangle_count(), 48);
        assert_eq!(e.status(), "subdivided");
        e.run_op_async("nope");
        settle(&mut e);
        assert!(e.status().contains("unknown operation"));
        assert!(e.undo());
        assert_eq!(e.mesh().triangle_count(), 12);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn export_and_reopen_in_background() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = ModelEditor::default();
        e.set_mesh(MeshData::sphere(1.0, 8, 6));
        let tris = e.mesh().triangle_count();
        for ext in ["obj", "gltf"] {
            e.export_path = dir.path().join(format!("m.{ext}")).to_string_lossy().into_owned();
            e.export();
            settle(&mut e);
            assert!(e.status().starts_with("saved"), "{}", e.status());
        }
        let gltf = std::fs::read_to_string(dir.path().join("m.gltf")).unwrap();
        assert!(gltf.contains("\"asset\""));
        e.set_mesh(MeshData::cube(1.0));
        e.path = dir.path().join("m.obj").to_string_lossy().into_owned();
        e.open();
        settle(&mut e);
        assert!(e.status().starts_with("opened"), "{}", e.status());
        assert_eq!(e.mesh().triangle_count(), tris);
        e.path = dir.path().join("missing.obj").to_string_lossy().into_owned();
        e.open();
        settle(&mut e);
        assert!(e.status().contains("missing.obj"));
    }

    #[test]
    fn opens_exported_obj() {
        let mut e = ModelEditor::default();
        let obj = ops::to_obj(&MeshData::sphere(1.0, 8, 6));
        e.open_file("x.obj", obj.as_bytes()).unwrap();
        assert!(e.mesh().triangle_count() > 12);
        assert!(e.open_file("x.fbx", b"").is_err());
    }
}
