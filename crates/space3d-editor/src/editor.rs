//! [`EditorPlugin`], [`EditorState`] and the main editor UI system.

use crate::components::{MeshSource, ensure_stable_id, register_editor_components};
use crate::files::{IoJobs, read_file_with, write_file_with};
use crate::hierarchy::{HierarchyAction, hierarchy_ui};
use crate::history::{EditCommand, EditorHistory};
use crate::inspector::{InspectorState, inspector_ui};
use crate::model_editor::ModelEditor;
use crate::scene_ops::{self, AddKind};
use crate::sim::{SimControl, SimControlPlugin};
use crate::texture_editor::TextureEditor;
use crate::viewport::Gizmo;
use space3d_core::glam::Vec3;
use space3d_core::hecs::{Entity, World};
use space3d_core::{App, Name, Plugin, Stage, SystemCtx, TickRate, Time, Transform};
use space3d_render::{Material, Mesh3d, RenderAssets, Renderer, TextureHandle};
use space3d_scene::{ComponentRegistry, Scene, SceneFormat, ScenePlugin};
use space3d_ui::{Console, DebugOverlay, EguiContext, Profiler, UiPlugin, set_tool_dock_area};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

/// Editor state (resource inserted by [`EditorPlugin`]).
pub struct EditorState {
    /// Selected entity.
    pub selected: Option<Entity>,
    /// Show the hierarchy panel.
    pub show_hierarchy: bool,
    /// Show the inspector panel.
    pub show_inspector: bool,
    /// Draw the editor UI at all (F1 toggles).
    pub visible: bool,
    /// Scene file for save/load (`.ron` or `.json`).
    pub scene_path: String,
    /// Tick count of the "Step N" button.
    pub step_count: u64,
    /// Hierarchy search text.
    pub hierarchy_filter: String,
    status: String,
    inspector: InspectorState,
    gizmo: Gizmo,
    scene_reads: IoJobs<Scene>,
    /// Pending scene writes, labelled with a note appended to the status when done.
    scene_writes: IoJobs<String>,
    mesh_fingerprints: HashMap<Entity, u64>,
    /// Textures the editor uploaded ("Use on selection"); freed when unused after the undo
    /// history is cleared (new/open scene).
    owned_textures: Vec<TextureHandle>,
    /// Texture-editor revision and texture of the last "Use on selection" (re-used while the
    /// image is unchanged).
    applied_texture: Option<(u64, TextureHandle)>,
}

impl std::fmt::Debug for EditorState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EditorState")
            .field("selected", &self.selected)
            .field("scene_path", &self.scene_path)
            .field("status", &self.status)
            .finish()
    }
}

impl EditorState {
    fn new(scene_path: String) -> Self {
        Self {
            selected: None,
            show_hierarchy: true,
            show_inspector: true,
            visible: true,
            scene_path,
            step_count: 10,
            hierarchy_filter: String::new(),
            status: String::new(),
            inspector: InspectorState::default(),
            gizmo: Gizmo::default(),
            scene_reads: IoJobs::default(),
            scene_writes: IoJobs::default(),
            mesh_fingerprints: HashMap::new(),
            owned_textures: Vec::new(),
            applied_texture: None,
        }
    }

    /// Last status message.
    pub fn status(&self) -> &str {
        &self.status
    }

    /// Show a status message.
    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status = msg.into();
        tracing::info!("editor: {}", self.status);
    }

    /// Whether a scene file operation is in flight.
    pub fn io_pending(&self) -> bool {
        !self.scene_reads.is_empty() || !self.scene_writes.is_empty()
    }
}

/// The in-app editor: hierarchy, inspector, gizmo + picking, play/pause/step, scene
/// save/load, undo/redo, texture editor and model editor. See the [crate docs](crate).
///
/// Adds [`UiPlugin`] (default settings) if no UI plugin exists yet, [`ScenePlugin`] and
/// [`SimControlPlugin`], and registers render components in the scene
/// [`ComponentRegistry`] ([`register_editor_components`]).
#[derive(Clone, Debug)]
pub struct EditorPlugin {
    /// Start with `FixedUpdate` paused.
    pub start_paused: bool,
    /// Default scene file.
    pub scene_path: String,
    /// Open the texture editor at startup.
    pub show_texture_editor: bool,
    /// Open the model editor at startup.
    pub show_model_editor: bool,
}

impl Default for EditorPlugin {
    fn default() -> Self {
        Self {
            start_paused: true,
            scene_path: "scene.ron".into(),
            show_texture_editor: false,
            show_model_editor: false,
        }
    }
}

impl Plugin for EditorPlugin {
    fn build(&self, app: &mut App) {
        if !app.has_plugin(std::any::type_name::<UiPlugin>()) {
            app.add_plugin(UiPlugin::default());
        }
        app.add_plugin(ScenePlugin);
        register_editor_components(app.resources.init::<ComponentRegistry>());
        app.add_plugin(SimControlPlugin {
            paused: self.start_paused,
        });
        app.init_resource::<RenderAssets>();
        app.insert_resource(EditorState::new(self.scene_path.clone()));
        app.init_resource::<EditorHistory>();
        let mut tex = TextureEditor::default();
        tex.open = self.show_texture_editor;
        app.insert_resource(tex);
        let mut model = ModelEditor::default();
        model.open = self.show_model_editor;
        app.insert_resource(model);
        app.add_system_named(Stage::PreUpdate, "editor::resolve_meshes", |sys| {
            scene_ops::resolve_mesh_sources(sys.world, sys.resources.get_mut::<RenderAssets>());
        });
        app.add_system_named(Stage::Update, "editor::ui", editor_ui_system);
    }
}

fn fingerprint(s: &MeshSource) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    match s {
        MeshSource::Data(m) => {
            ("data", m.positions.len(), m.indices.len()).hash(&mut h);
            for p in m.positions.iter().take(8).chain(m.positions.iter().rev().take(8)) {
                p.map(f32::to_bits).hash(&mut h);
            }
        }
        other => format!("{other:?}").hash(&mut h),
    }
    h.finish()
}

/// Rebuild GPU meshes whose [`MeshSource`] changed (edits, undo/redo).
fn sync_mesh_sources(world: &World, assets: Option<&mut RenderAssets>, prints: &mut HashMap<Entity, u64>) {
    let Some(assets) = assets else { return };
    let mut seen = HashMap::with_capacity(prints.len());
    for (e, source, mesh) in world.query::<(Entity, &MeshSource, &Mesh3d)>().iter() {
        let fp = fingerprint(source);
        if prints.get(&e).is_some_and(|&old| old != fp) {
            assets.update_mesh(mesh.0, source.mesh());
        }
        seen.insert(e, fp);
    }
    *prints = seen;
}

fn with_registry<R>(
    sys: &mut SystemCtx,
    f: impl FnOnce(&mut World, &ComponentRegistry, &mut space3d_core::Resources) -> R,
) -> Option<R> {
    let registry = sys.resources.remove::<ComponentRegistry>()?;
    let out = f(sys.world, &registry, sys.resources);
    sys.resources.insert(registry);
    Some(out)
}

struct Tools<'a> {
    state: &'a mut EditorState,
    history: &'a mut EditorHistory,
    tex: &'a mut TextureEditor,
    model: &'a mut ModelEditor,
}

impl Tools<'_> {
    fn undo(&mut self, sys: &mut SystemCtx) {
        let label = with_registry(sys, |w, r, _| self.history.undo(w, r)).flatten();
        match label {
            Some(l) => self.state.set_status(format!("undo: {l}")),
            None => self.state.set_status("nothing to undo"),
        }
    }

    fn redo(&mut self, sys: &mut SystemCtx) {
        let label = with_registry(sys, |w, r, _| self.history.redo(w, r)).flatten();
        match label {
            Some(l) => self.state.set_status(format!("redo: {l}")),
            None => self.state.set_status("nothing to redo"),
        }
    }

    /// Serialize the scene now (it needs the world) and encode + write it in the background.
    fn save_scene(&mut self, sys: &mut SystemCtx) {
        let path = self.state.scene_path.clone();
        let result = with_registry(sys, |w, r, _| {
            scene_ops::save_scene(w, r).map(|scene| (scene, scene_ops::session_only_render_state(w, r)))
        });
        match result {
            Some(Ok((scene, session_only))) => {
                let format = SceneFormat::from_path(&path);
                let note = if session_only > 0 {
                    format!(
                        " · {session_only} entities have a mesh without MeshSource or a texture: \
                         kept in this session only"
                    )
                } else {
                    String::new()
                };
                self.state.set_status(format!("saving {path}..."));
                self.state.scene_writes.push(
                    note,
                    write_file_with(path, move || scene.to_bytes(format).map_err(|e| e.to_string())),
                );
            }
            Some(Err(e)) => self.state.set_status(format!("save failed: {e}")),
            None => self.state.set_status("no component registry"),
        }
    }

    /// Read + parse the scene file in the background; [`Self::poll_io`] swaps it in.
    fn open_scene(&mut self) {
        let path = self.state.scene_path.clone();
        self.state.set_status(format!("loading {path}..."));
        let task = read_file_with(path.clone(), |path, bytes| {
            Scene::from_bytes(&bytes, SceneFormat::from_path(path)).map_err(|e| format!("{path}: {e}"))
        });
        self.state.scene_reads.push(path, task);
    }

    fn new_scene(&mut self, sys: &mut SystemCtx) {
        with_registry(sys, |w, r, res| {
            scene_ops::clear_scene(w, r, res.get_mut::<RenderAssets>())
        });
        self.history.clear();
        self.state.selected = None;
        self.free_unused_textures(sys);
        self.state.set_status("new scene");
    }

    /// Free editor-uploaded textures no material uses (only valid right after the undo history
    /// was cleared: history entries may reference textures).
    fn free_unused_textures(&mut self, sys: &mut SystemCtx) {
        let used: HashSet<TextureHandle> = sys
            .world
            .query::<&Material>()
            .iter()
            .filter_map(|m| m.base_color_texture)
            .collect();
        let Some(assets) = sys.resources.get_mut::<RenderAssets>() else {
            return;
        };
        self.state.owned_textures.retain(|t| {
            let keep = used.contains(t);
            if !keep {
                assets.remove_texture(*t);
            }
            keep
        });
        if self
            .state
            .applied_texture
            .is_some_and(|(_, t)| !self.state.owned_textures.contains(&t))
        {
            self.state.applied_texture = None;
        }
    }

    fn poll_io(&mut self, sys: &mut SystemCtx) {
        for (path, loaded) in self.state.scene_reads.poll() {
            match loaded {
                Ok(scene) => {
                    let result = with_registry(sys, |w, r, res| {
                        scene_ops::load_scene(w, r, res.get_mut::<RenderAssets>(), &scene)
                    });
                    match result {
                        Some(Ok(spawned)) => {
                            self.history.clear();
                            self.state.selected = None;
                            self.free_unused_textures(sys);
                            self.state
                                .set_status(format!("loaded {path} ({} entities)", spawned.len()));
                        }
                        Some(Err(e)) => self.state.set_status(format!("load failed: {e}")),
                        None => self.state.set_status("no component registry"),
                    }
                }
                Err(e) => self.state.set_status(format!("load failed: {e}")),
            }
        }
        for (note, result) in self.state.scene_writes.poll() {
            let msg = match result {
                Ok(msg) => msg + &note,
                Err(e) => format!("save failed: {e}"),
            };
            self.state.set_status(msg);
        }
    }

    fn add(&mut self, sys: &mut SystemCtx, kind: AddKind, parent: Option<Entity>) {
        let e = scene_ops::add_entity(sys, self.history, kind, parent);
        self.state.selected = Some(e);
        self.state.set_status(format!("added {}", kind.label()));
    }

    fn delete_selected(&mut self, sys: &mut SystemCtx) {
        if let Some(e) = self.state.selected.take() {
            scene_ops::delete_entity(sys, self.history, e);
            self.state.set_status("deleted");
        }
    }

    fn duplicate_selected(&mut self, sys: &mut SystemCtx) {
        if let Some(e) = self.state.selected
            && let Some(copy) = scene_ops::duplicate_entity(sys, self.history, e)
        {
            self.state.selected = Some(copy);
            self.state.set_status("duplicated");
        }
    }

    fn apply_hierarchy(&mut self, sys: &mut SystemCtx, action: HierarchyAction) {
        match action {
            HierarchyAction::Select(e) => self.state.selected = Some(e),
            HierarchyAction::Delete(e) => {
                scene_ops::delete_entity(sys, self.history, e);
                if self.state.selected == Some(e) {
                    self.state.selected = None;
                }
            }
            HierarchyAction::Duplicate(e) => {
                self.state.selected = Some(e);
                self.duplicate_selected(sys);
            }
            HierarchyAction::Add(kind, parent) => self.add(sys, kind, parent),
            HierarchyAction::Reparent { child, parent } => {
                let Some(id) = ensure_stable_id(sys.world, child) else {
                    return;
                };
                let before = sys
                    .world
                    .get::<&space3d_core::Parent>(child)
                    .ok()
                    .map(|p| p.0)
                    .and_then(|p| ensure_stable_id(sys.world, p));
                let after = parent.and_then(|p| ensure_stable_id(sys.world, p));
                if before != after {
                    let history = &mut *self.history;
                    with_registry(sys, |w, r, _| {
                        history.execute(EditCommand::reparent(id, before, after), w, r)
                    });
                    self.state.set_status("reparented");
                }
            }
        }
    }

    /// Texture editor "use on selection": set the image as the selected entity's material
    /// texture (undoable). The uploaded texture is re-used while the image is unchanged.
    fn texture_to_selection(&mut self, sys: &mut SystemCtx) {
        self.tex.apply_to_selection = false;
        let (Some(e), true) = (self.state.selected, self.tex.image().is_some()) else {
            self.state.set_status("select an entity with a material first");
            return;
        };
        let Some(id) = ensure_stable_id(sys.world, e) else {
            return;
        };
        let Some(assets) = sys.resources.get_mut::<RenderAssets>() else {
            return;
        };
        let revision = self.tex.revision();
        let handle = match self.state.applied_texture {
            Some((rev, handle)) if rev == revision && self.state.owned_textures.contains(&handle) => handle,
            _ => {
                let Some(img) = self.tex.image().cloned() else { return };
                let handle = assets.add_image(img);
                self.state.owned_textures.push(handle);
                self.state.applied_texture = Some((revision, handle));
                handle
            }
        };
        let before = sys.world.get::<&Material>(e).ok().map(|m| (*m).clone());
        let mut after = before.clone().unwrap_or_default();
        after.base_color_texture = Some(handle);
        let _ = sys.world.insert_one(e, after.clone());
        self.history
            .push(EditCommand::set("Set texture", id, before, Some(after)));
        self.state.set_status("texture applied to selection");
    }

    /// Model editor "add to scene" (undoable).
    fn model_to_scene(&mut self, sys: &mut SystemCtx) {
        self.model.add_to_scene = false;
        let mesh = self.model.mesh().clone();
        let color = self.model.color;
        let Some(assets) = sys.resources.get_mut::<RenderAssets>() else {
            return;
        };
        let handle = assets.add_mesh(mesh.clone());
        let e = sys.world.spawn((
            Name("Model".into()),
            Transform::IDENTITY,
            MeshSource::Data(mesh),
            Mesh3d(handle),
            Material::color(color.into()),
        ));
        scene_ops::record_spawn(sys, self.history, "Add model", &[e]);
        self.state.selected = Some(e);
        self.state.set_status("model added to scene");
    }
}

fn menu_bar(ui: &mut egui::Ui, sys: &mut SystemCtx, t: &mut Tools) {
    let frame = crate::widgets::menu_bar_frame(ui.style());
    egui::Panel::top("space3d_editor_menu").frame(frame).show(ui, |ui| {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button("New scene").clicked() {
                    t.new_scene(sys);
                    ui.close();
                }
                ui.horizontal(|ui| {
                    ui.label("file");
                    ui.text_edit_singleline(&mut t.state.scene_path);
                });
                if ui.button("Open").clicked() {
                    t.open_scene();
                    ui.close();
                }
                if ui.button("Save").clicked() {
                    t.save_scene(sys);
                    ui.close();
                }
                if ui.button("Quit").clicked() {
                    sys.send(space3d_core::AppExit);
                }
            });
            ui.menu_button("Edit", |ui| {
                let undo = t
                    .history
                    .undo_label()
                    .map(|l| format!("Undo {l}"))
                    .unwrap_or("Undo".into());
                if ui.add_enabled(t.history.can_undo(), egui::Button::new(undo)).clicked() {
                    t.undo(sys);
                    ui.close();
                }
                let redo = t
                    .history
                    .redo_label()
                    .map(|l| format!("Redo {l}"))
                    .unwrap_or("Redo".into());
                if ui.add_enabled(t.history.can_redo(), egui::Button::new(redo)).clicked() {
                    t.redo(sys);
                    ui.close();
                }
                ui.separator();
                let has_sel = t.state.selected.is_some();
                if ui.add_enabled(has_sel, egui::Button::new("Duplicate")).clicked() {
                    t.duplicate_selected(sys);
                    ui.close();
                }
                if ui.add_enabled(has_sel, egui::Button::new("Delete")).clicked() {
                    t.delete_selected(sys);
                    ui.close();
                }
            });
            ui.menu_button("Add", |ui| {
                for kind in AddKind::ALL {
                    if ui.button(kind.label()).clicked() {
                        t.add(sys, kind, None);
                        ui.close();
                    }
                }
            });
            ui.menu_button("Window", |ui| {
                ui.checkbox(&mut t.state.show_hierarchy, "Hierarchy");
                ui.checkbox(&mut t.state.show_inspector, "Inspector");
                ui.checkbox(&mut t.tex.open, "Texture editor");
                ui.checkbox(&mut t.model.open, "Model editor");
                if let Some(d) = sys.resources.get_mut::<DebugOverlay>() {
                    ui.checkbox(&mut d.visible, "Debug overlay (F3)");
                }
                if let Some(p) = sys.resources.get_mut::<Profiler>() {
                    ui.checkbox(&mut p.visible, "Profiler (F4)");
                }
                if let Some(c) = sys.resources.get_mut::<Console>() {
                    ui.checkbox(&mut c.visible, "Console (`)");
                }
            });
            ui.separator();
            sim_controls(ui, sys, t.state);
            ui.separator();
            ui.label(egui::RichText::new(&t.state.status).weak());
        });
    });
}

fn sim_controls(ui: &mut egui::Ui, sys: &mut SystemCtx, state: &mut EditorState) {
    let (tick, hz) = (
        sys.resources.get::<Time>().map_or(0, |t| t.tick),
        sys.resources.get::<TickRate>().map_or(0.0, |r| r.hz),
    );
    let Some(sim) = sys.resources.get_mut::<SimControl>() else {
        return;
    };
    let label = if sim.is_paused() { "Play" } else { "Pause" };
    if ui.button(label).on_hover_text("Space").clicked() {
        sim.toggle();
    }
    if ui.button("Step").on_hover_text("Run one fixed tick (.)").clicked() {
        sim.step(1);
    }
    ui.add(
        egui::DragValue::new(&mut state.step_count)
            .range(1..=100_000)
            .suffix(" ticks"),
    );
    if ui.button("Step N").clicked() {
        sim.step(state.step_count);
    }
    let run = if sim.is_paused() { "paused" } else { "running" };
    ui.label(format!("tick {tick} @ {hz} Hz · {run}"));
}

fn shortcuts(ctx: &egui::Context, sys: &mut SystemCtx, t: &mut Tools) {
    if ctx.egui_wants_keyboard_input() {
        return;
    }
    use egui::{Key, KeyboardShortcut, Modifiers};
    let pressed = |m: Modifiers, k: Key| ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(m, k)));
    if pressed(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z) || pressed(Modifiers::COMMAND, Key::Y) {
        t.redo(sys);
    } else if pressed(Modifiers::COMMAND, Key::Z) {
        t.undo(sys);
    }
    if pressed(Modifiers::COMMAND, Key::S) {
        t.save_scene(sys);
    }
    if pressed(Modifiers::COMMAND, Key::O) {
        t.open_scene();
    }
    if pressed(Modifiers::COMMAND, Key::D) {
        t.duplicate_selected(sys);
    }
    if pressed(Modifiers::NONE, Key::Delete) {
        t.delete_selected(sys);
    }
    if pressed(Modifiers::NONE, Key::F1) {
        t.state.visible = !t.state.visible;
    }
    if let Some(sim) = sys.resources.get_mut::<SimControl>() {
        if pressed(Modifiers::NONE, Key::Space) {
            sim.toggle();
        }
        if pressed(Modifiers::NONE, Key::Period) {
            sim.step(1);
        }
    }
}

/// `Stage::Update`: the whole editor UI.
pub(crate) fn editor_ui_system(sys: &mut SystemCtx) {
    let Some(ctx) = EguiContext::active_context(sys) else {
        return;
    };
    let (Some(mut state), Some(mut history), Some(mut tex), Some(mut model)) = (
        sys.resources.remove::<EditorState>(),
        sys.resources.remove::<EditorHistory>(),
        sys.resources.remove::<TextureEditor>(),
        sys.resources.remove::<ModelEditor>(),
    ) else {
        return;
    };
    {
        let mut t = Tools {
            state: &mut state,
            history: &mut history,
            tex: &mut tex,
            model: &mut model,
        };
        t.poll_io(sys);
        if t.state.selected.is_some_and(|e| !sys.world.contains(e)) {
            t.state.selected = None;
        }
        shortcuts(&ctx, sys, &mut t);

        let mut viewport = ctx.content_rect();
        if t.state.visible {
            EguiContext::with_root_ui(sys, |ui, sys| {
                menu_bar(ui, sys, &mut t);
                if t.state.show_hierarchy {
                    let mut actions = Vec::new();
                    let frame = crate::widgets::panel_frame(ui.style());
                    egui::Panel::left("space3d_editor_hierarchy")
                        .default_size(210.0)
                        .size_range(160.0..=400.0)
                        .frame(frame)
                        .show(ui, |ui| {
                            let count = crate::hierarchy::entity_count(sys.world);
                            crate::widgets::panel_title(ui, "Hierarchy", &format!("{count} entities"));
                            crate::widgets::visible_scroll_bars(ui);
                            actions = hierarchy_ui(ui, sys.world, t.state.selected, &mut t.state.hierarchy_filter);
                        });
                    for a in actions {
                        t.apply_hierarchy(sys, a);
                    }
                }
                if t.state.show_inspector {
                    let frame = crate::widgets::panel_frame(ui.style());
                    egui::Panel::right("space3d_editor_inspector")
                        .default_size(280.0)
                        .size_range(220.0..=480.0)
                        .frame(frame)
                        .show(ui, |ui| {
                            crate::widgets::panel_title(ui, "Inspector", "");
                            crate::widgets::visible_scroll_bars(ui);
                            // Full width whatever is selected: the panel keeps its width.
                            egui::ScrollArea::vertical()
                                .id_salt("space3d_inspector_scroll")
                                .auto_shrink([false, true])
                                .show(ui, |ui| {
                                    match (t.state.selected, sys.resources.get::<ComponentRegistry>()) {
                                        (Some(e), Some(registry)) => {
                                            // MeshSource edits are picked up by `sync_mesh_sources`.
                                            let _mesh_changed = inspector_ui(
                                                ui,
                                                sys.world,
                                                registry,
                                                sys.resources.get::<RenderAssets>(),
                                                t.history,
                                                &mut t.state.inspector,
                                                e,
                                            );
                                        }
                                        _ => {
                                            ui.label("Select an entity (hierarchy or click in the viewport).");
                                        }
                                    }
                                });
                        });
                }
                viewport = ui.available_rect_before_wrap();
            });
            // Over the viewport: never over the menu bar or the side panels.
            t.tex.show_in(&ctx, viewport);
            t.model.show_in(&ctx, viewport);
        } else {
            t.tex.poll_io();
            t.model.poll_io();
        }
        if t.tex.apply_to_selection {
            t.texture_to_selection(sys);
        }
        if t.model.add_to_scene {
            t.model_to_scene(sys);
        }
        if t.model.open {
            t.model.sync_preview(sys.world, sys.resources, &ctx);
        } else {
            // Free the preview target and entities while the window is closed.
            t.model.destroy_preview(sys.world, sys.resources, &ctx);
        }

        // The engine tools (F3 / F4 / console) dock inside the viewport, clear of the menu bar
        // and the side panels (the whole screen while the editor UI is hidden).
        set_tool_dock_area(sys.resources, viewport);

        // Viewport: gizmo + picking.
        if t.state.visible {
            let aspect = sys
                .resources
                .get::<Renderer>()
                .map(|r| r.size())
                .map_or(16.0 / 9.0, |(w, h)| w as f32 / h.max(1) as f32);
            let assets = sys.resources.get::<RenderAssets>();
            let outcome =
                t.state
                    .gizmo
                    .interact(&ctx, viewport, sys.world, assets, t.history, t.state.selected, aspect);
            if let Some(sel) = outcome.select {
                t.state.selected = sel;
            }
        }
        sync_mesh_sources(
            sys.world,
            sys.resources.get_mut::<RenderAssets>(),
            &mut t.state.mesh_fingerprints,
        );
    }
    sys.resources.insert(state);
    sys.resources.insert(history);
    sys.resources.insert(tex);
    sys.resources.insert(model);
}

/// Look at `target` from `eye` (convenience for editor setups).
pub fn camera_transform(eye: Vec3, target: Vec3) -> Transform {
    Transform::from_xyz(eye.x, eye.y, eye.z).looking_at(target, Vec3::Y)
}
