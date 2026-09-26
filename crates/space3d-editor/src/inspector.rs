//! Inspector panel: typed editors for `Name`, `Transform`, `Parent`, `Material`, lights,
//! `Camera3d`, `Visibility`, `MeshSource`, `Mesh3d`, the graphics components (`CameraFrame`,
//! `PostProcess`, `Skybox`, `EnvironmentLight`, `DirectionalShadow`), plus a JSON editor for
//! every other component registered in the scene `ComponentRegistry`. All edits are undoable.

use crate::components::{
    ANTI_ALIASING, HDR_MODES, MeshSource, SHADOW_FILTERS, TONEMAPPERS, can_remove, component_values, ensure_stable_id,
    names, remove_component, set_component_value,
};
use crate::history::{EditCommand, EditorHistory};
use space3d_core::glam::{EulerRot, Quat, Vec3, Vec4};
use space3d_core::hecs::{Component, Entity, World};
use space3d_core::{Name, Parent, Transform};
use space3d_render::{
    Camera3d, CameraFrame, DirectionalLight, DirectionalShadow, EnvironmentLight, Material, Mesh3d, PointLight,
    PostProcess, Projection, RenderAssets, RenderTarget, Skybox, Visibility,
};
use space3d_scene::{ComponentRegistry, ComponentValue, StableId};
use std::collections::{BTreeMap, HashMap};

/// Per-selection inspector state.
#[derive(Debug, Default)]
pub struct InspectorState {
    /// JSON text being edited, by component name.
    json_edits: HashMap<String, String>,
    /// Last JSON error by component name.
    json_errors: HashMap<String, String>,
    /// Euler angles shown for the rotation (avoids quaternion → euler jitter while dragging).
    euler: Option<(Entity, Quat, Vec3)>,
    /// Component name picked in "Add component".
    add_pick: String,
    entity: Option<Entity>,
}

/// Handled by typed editors (not shown in the JSON section).
const TYPED: [&str; 12] = [
    "Name",
    space3d_scene::TRANSFORM,
    space3d_scene::PARENT,
    names::MATERIAL,
    names::DIRECTIONAL_LIGHT,
    names::POINT_LIGHT,
    names::CAMERA,
    names::VISIBILITY,
    names::MESH_SOURCE,
    names::CAMERA_FRAME,
    names::POST_PROCESS,
    names::DIRECTIONAL_SHADOW,
];

/// Not offered in "Add component".
const HIDDEN: [&str; 3] = ["StableId", "GlobalTransform", "Mesh3d"];

/// Did an edit gesture end (drag released, text field left, click)?
fn gesture_ended(r: &egui::Response) -> bool {
    r.drag_stopped() || r.lost_focus() || (r.changed() && !r.dragged() && !r.has_focus())
}

fn vec3_edit(ui: &mut egui::Ui, v: &mut Vec3, speed: f64) -> egui::Response {
    ui.horizontal(|ui| {
        let x = ui.add(egui::DragValue::new(&mut v.x).speed(speed).prefix("x ").max_decimals(4));
        let y = ui.add(egui::DragValue::new(&mut v.y).speed(speed).prefix("y ").max_decimals(4));
        let z = ui.add(egui::DragValue::new(&mut v.z).speed(speed).prefix("z ").max_decimals(4));
        x | y | z
    })
    .inner
}

// egui's color buttons round-trip the value through HSVA every frame; writing that back
// unconditionally would nudge colors (and record bogus undo steps) just by showing them.
fn color3_edit(ui: &mut egui::Ui, c: &mut Vec3) -> egui::Response {
    let mut rgb = c.to_array();
    let r = ui.color_edit_button_rgb(&mut rgb);
    if r.changed() {
        *c = Vec3::from(rgb);
    }
    r
}

/// A combo box over a `(value, name)` table; returns the response of the box.
fn choice_edit<T: PartialEq + Copy>(ui: &mut egui::Ui, label: &str, v: &mut T, table: &[(T, &str)]) -> egui::Response {
    let current = table.iter().find(|(t, _)| *t == *v).map_or("(other)", |(_, n)| n);
    let mut r = egui::ComboBox::from_label(label)
        .selected_text(current)
        .show_ui(ui, |ui| {
            let mut changed: Option<egui::Response> = None;
            for (t, n) in table {
                let item = ui.selectable_label(*t == *v, *n);
                if item.clicked() {
                    *v = *t;
                }
                changed = Some(match changed {
                    Some(c) => c | item,
                    None => item,
                });
            }
            changed
        });
    if let Some(Some(inner)) = r.inner.take() {
        r.response |= inner;
    }
    r.response
}

/// Yaw (degrees about +Y) of a rotation, edited as a slider.
fn yaw_edit(ui: &mut egui::Ui, q: &mut Quat) -> egui::Response {
    let (yaw, _, _) = q.to_euler(EulerRot::YXZ);
    let mut deg = yaw.to_degrees();
    let r = ui.add(egui::Slider::new(&mut deg, -180.0..=180.0).text("yaw°"));
    if r.changed() {
        *q = Quat::from_rotation_y(deg.to_radians());
    }
    r
}

fn color4_edit(ui: &mut egui::Ui, c: &mut Vec4) -> egui::Response {
    let mut rgba = egui::Rgba::from_rgba_unmultiplied(c.x, c.y, c.z, c.w);
    let r = egui::color_picker::color_edit_button_rgba(ui, &mut rgba, egui::color_picker::Alpha::OnlyBlend);
    if r.changed() {
        *c = Vec4::from(rgba.to_rgba_unmultiplied());
    }
    r
}

/// Collapsible typed editor. Writes changes to the world and records merged undo steps.
#[allow(clippy::too_many_arguments)]
fn typed_section<T: Component + Clone + PartialEq>(
    ui: &mut egui::Ui,
    world: &mut World,
    registry: &ComponentRegistry,
    history: &mut EditorHistory,
    e: Entity,
    id: StableId,
    title: &str,
    body: impl FnOnce(&mut egui::Ui, &mut T) -> Option<egui::Response>,
) -> bool {
    let Ok(before) = world.get::<&T>(e).map(|c| (*c).clone()) else {
        return false;
    };
    let mut value = before.clone();
    let mut remove = false;
    let mut ended = false;
    egui::CollapsingHeader::new(title)
        .id_salt(("space3d_inspector", title))
        .default_open(true)
        .show(ui, |ui| {
            if let Some(r) = body(ui, &mut value) {
                ended = gesture_ended(&r);
            }
            if ui.small_button("Remove component").clicked() {
                remove = true;
            }
        });
    if remove {
        history.execute(
            EditCommand::set::<T>(format!("Remove {title}"), id, Some(before), None),
            world,
            registry,
        );
        return true;
    }
    let changed = value != before;
    if changed {
        let _ = world.insert_one(e, value.clone());
        history.push(
            EditCommand::set(format!("Edit {title}"), id, Some(before), Some(value)).merge_key(format!("{title}:{id}")),
        );
    }
    if ended {
        history.end_merge();
    }
    changed
}

/// Draw the inspector for `e`. Returns `true` if a `MeshSource` changed (mesh needs rebuild).
pub fn inspector_ui(
    ui: &mut egui::Ui,
    world: &mut World,
    registry: &ComponentRegistry,
    assets: Option<&RenderAssets>,
    history: &mut EditorHistory,
    state: &mut InspectorState,
    e: Entity,
) -> bool {
    if state.entity != Some(e) {
        state.json_edits.clear();
        state.json_errors.clear();
        state.entity = Some(e);
        history.end_merge();
    }
    let Some(id) = ensure_stable_id(world, e) else {
        ui.label("entity no longer exists");
        return false;
    };
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(format!("Entity #{}", e.id())).strong());
        ui.label(egui::RichText::new(id.to_string()).weak().small());
    });

    // Name.
    let before = world.get::<&Name>(e).ok().map(|n| n.0.clone());
    let mut text = before.clone().unwrap_or_default();
    let r = ui
        .horizontal(|ui| {
            ui.label("Name");
            ui.text_edit_singleline(&mut text)
        })
        .inner;
    if r.changed() {
        let after = (!text.is_empty()).then(|| Name(text.clone()));
        match &after {
            Some(n) => {
                let _ = world.insert_one(e, n.clone());
            }
            None => {
                let _ = world.remove_one::<Name>(e);
            }
        }
        history.push(EditCommand::set("Rename", id, before.map(Name), after).merge_key(format!("name:{id}")));
    }
    if r.lost_focus() {
        history.end_merge();
    }

    // Parent.
    if let Ok(p) = world.get::<&Parent>(e).map(|p| p.0) {
        let pname = crate::components::display_name(world, p);
        let mut unparent = false;
        ui.horizontal(|ui| {
            ui.label(format!("Parent: {pname}"));
            unparent = ui.small_button("Unparent").clicked();
        });
        if unparent {
            let pid = ensure_stable_id(world, p);
            history.execute(EditCommand::reparent(id, pid, None), world, registry);
        }
    }
    ui.separator();

    typed_section::<Transform>(ui, world, registry, history, e, id, "Transform", |ui, t| {
        let mut r = ui.label("translation");
        r |= vec3_edit(ui, &mut t.translation, 0.01);
        let euler = match state.euler {
            Some((ent, q, eu)) if ent == e && q == t.rotation => eu,
            _ => {
                let (x, y, z) = t.rotation.to_euler(EulerRot::XYZ);
                Vec3::new(x.to_degrees(), y.to_degrees(), z.to_degrees())
            }
        };
        let mut eu = euler;
        ui.label("rotation (deg)");
        let rr = vec3_edit(ui, &mut eu, 0.5);
        if eu != euler {
            t.rotation = Quat::from_euler(EulerRot::XYZ, eu.x.to_radians(), eu.y.to_radians(), eu.z.to_radians());
        }
        state.euler = Some((e, t.rotation, eu));
        r |= rr;
        ui.label("scale");
        r |= vec3_edit(ui, &mut t.scale, 0.01);
        Some(r)
    });

    typed_section::<Material>(ui, world, registry, history, e, id, "Material", |ui, m| {
        let mut r = ui
            .horizontal(|ui| {
                ui.label("base color");
                color4_edit(ui, &mut m.base_color)
            })
            .inner;
        r |= ui.add(egui::Slider::new(&mut m.metallic, 0.0..=1.0).text("metallic"));
        r |= ui.add(egui::Slider::new(&mut m.roughness, 0.0..=1.0).text("roughness"));
        r |= ui
            .horizontal(|ui| {
                ui.label("emissive");
                color3_edit(ui, &mut m.emissive)
            })
            .inner;
        r |= ui.checkbox(&mut m.unlit, "unlit");
        r |= ui.checkbox(&mut m.double_sided, "double sided");
        r |= ui.checkbox(&mut m.alpha_blend, "alpha blend");
        if let Some(t) = m.base_color_texture {
            ui.horizontal(|ui| {
                ui.label(format!("texture #{}", t.index()));
                if ui.small_button("Clear").clicked() {
                    m.base_color_texture = None;
                }
            });
        }
        Some(r)
    });

    typed_section::<DirectionalLight>(ui, world, registry, history, e, id, "Directional light", |ui, l| {
        let mut r = ui
            .horizontal(|ui| {
                ui.label("color");
                color3_edit(ui, &mut l.color)
            })
            .inner;
        r |= ui.add(
            egui::DragValue::new(&mut l.intensity)
                .speed(0.05)
                .range(0.0..=1000.0)
                .prefix("intensity "),
        );
        Some(r)
    });

    typed_section::<PointLight>(ui, world, registry, history, e, id, "Point light", |ui, l| {
        let mut r = ui
            .horizontal(|ui| {
                ui.label("color");
                color3_edit(ui, &mut l.color)
            })
            .inner;
        r |= ui.add(
            egui::DragValue::new(&mut l.intensity)
                .speed(0.05)
                .range(0.0..=1000.0)
                .prefix("intensity "),
        );
        r |= ui.add(
            egui::DragValue::new(&mut l.range)
                .speed(0.1)
                .range(0.01..=10_000.0)
                .prefix("range "),
        );
        Some(r)
    });

    typed_section::<Camera3d>(ui, world, registry, history, e, id, "Camera", |ui, c| {
        let mut ortho = matches!(c.projection, Projection::Orthographic { .. });
        let mut r = ui.checkbox(&mut ortho, "orthographic");
        c.projection = match (ortho, c.projection) {
            (true, Projection::Perspective { near, far, .. }) => Projection::Orthographic {
                height: 10.0,
                near,
                far,
            },
            (false, Projection::Orthographic { near, far, .. }) => Projection::Perspective {
                fov_y: 60f32.to_radians(),
                near,
                far,
            },
            (_, p) => p,
        };
        match &mut c.projection {
            Projection::Perspective { fov_y, near, far } => {
                let mut deg = fov_y.to_degrees();
                r |= ui.add(egui::Slider::new(&mut deg, 5.0..=170.0).text("fov°"));
                *fov_y = deg.to_radians();
                r |= ui.add(
                    egui::DragValue::new(near)
                        .speed(0.01)
                        .range(0.0001..=100.0)
                        .prefix("near "),
                );
                r |= ui.add(egui::DragValue::new(far).speed(1.0).range(0.01..=1e6).prefix("far "));
            }
            Projection::Orthographic { height, near, far } => {
                r |= ui.add(
                    egui::DragValue::new(height)
                        .speed(0.1)
                        .range(0.01..=1e5)
                        .prefix("height "),
                );
                r |= ui.add(egui::DragValue::new(near).speed(0.01).prefix("near "));
                r |= ui.add(egui::DragValue::new(far).speed(1.0).prefix("far "));
            }
        }
        let mut clear = c.clear_color.is_some();
        r |= ui.checkbox(&mut clear, "clear");
        match (clear, &mut c.clear_color) {
            (true, Some(col)) => r |= color4_edit(ui, col),
            (true, None) => c.clear_color = Some(Vec4::new(0.02, 0.02, 0.03, 1.0)),
            (false, _) => c.clear_color = None,
        }
        r |= ui.add(egui::DragValue::new(&mut c.order).prefix("order "));
        r |= ui.checkbox(&mut c.active, "active");
        if let RenderTarget::Texture(t) = c.target {
            ui.label(format!("renders to texture #{}", t.index()));
        }
        Some(r)
    });

    typed_section::<CameraFrame>(ui, world, registry, history, e, id, "Camera frame", |ui, f| {
        let mut r = choice_edit(ui, "HDR", &mut f.hdr, &HDR_MODES);
        let mut own = f.render_scale.is_some();
        r |= ui.checkbox(&mut own, "own render scale");
        match (own, &mut f.render_scale) {
            (true, Some(scale)) => r |= ui.add(egui::Slider::new(scale, 0.25..=1.0).text("render scale")),
            (true, None) => f.render_scale = Some(1.0),
            (false, _) => f.render_scale = None,
        }
        Some(r)
    });

    typed_section::<PostProcess>(ui, world, registry, history, e, id, "Post-processing", |ui, p| {
        let mut r = ui.checkbox(&mut p.enabled, "enabled");
        r |= ui.add(
            egui::DragValue::new(&mut p.exposure)
                .speed(0.05)
                .range(-16.0..=16.0)
                .prefix("exposure (EV) "),
        );
        r |= ui.checkbox(&mut p.auto_exposure.enabled, "auto exposure");
        r |= choice_edit(ui, "tonemapper", &mut p.tonemapper, &TONEMAPPERS);
        r |= ui.checkbox(&mut p.bloom.enabled, "bloom");
        r |= ui.add(egui::Slider::new(&mut p.bloom.intensity, 0.0..=2.0).text("bloom intensity"));
        r |= ui.add(egui::Slider::new(&mut p.bloom.threshold, 0.0..=8.0).text("bloom threshold"));
        r |= choice_edit(ui, "anti-aliasing", &mut p.anti_aliasing, &ANTI_ALIASING);
        r |= ui.checkbox(&mut p.vignette.enabled, "vignette");
        r |= ui.add(egui::Slider::new(&mut p.vignette.intensity, 0.0..=1.0).text("vignette intensity"));
        let mut ca = if p.chromatic_aberration.enabled {
            p.chromatic_aberration.intensity
        } else {
            0.0
        };
        let ca_r = ui.add(egui::Slider::new(&mut ca, 0.0..=1.0).text("chromatic aberration"));
        if ca_r.changed() {
            p.chromatic_aberration.enabled = ca > 0.0;
            p.chromatic_aberration.intensity = ca;
        }
        r |= ca_r;
        r |= ui.checkbox(&mut p.film_grain.enabled, "film grain");
        r |= ui.add(egui::Slider::new(&mut p.film_grain.intensity, 0.0..=0.5).text("grain intensity"));
        r |= ui.checkbox(&mut p.distortion.enabled, "distortion");
        r |= ui.add(egui::Slider::new(&mut p.distortion.strength, 0.0..=4.0).text("distortion strength"));
        r |= ui.add(egui::Slider::new(&mut p.color_grading.saturation, 0.0..=2.0).text("saturation"));
        r |= ui.add(egui::Slider::new(&mut p.color_grading.contrast, 0.5..=2.0).text("contrast"));
        r |= ui.add(egui::Slider::new(&mut p.color_grading.temperature, -1.0..=1.0).text("temperature"));
        r |= ui.checkbox(&mut p.dither, "dither");
        if p.color_grading.lut.is_some() {
            ui.label("grades through a 3D LUT (session-local: not saved)");
        }
        Some(r)
    });

    typed_section::<Skybox>(ui, world, registry, history, e, id, "Skybox", |ui, sky| {
        ui.label(format!("cubemap #{}", sky.cubemap.index()));
        let mut r = ui.add(
            egui::DragValue::new(&mut sky.intensity)
                .speed(0.01)
                .range(0.0..=100.0)
                .prefix("intensity "),
        );
        r |= yaw_edit(ui, &mut sky.rotation);
        Some(r)
    });

    typed_section::<EnvironmentLight>(ui, world, registry, history, e, id, "Environment light", |ui, env| {
        ui.label(format!("cubemap #{}", env.cubemap.index()));
        let mut r = ui.add(
            egui::DragValue::new(&mut env.diffuse_intensity)
                .speed(0.01)
                .range(0.0..=100.0)
                .prefix("diffuse "),
        );
        r |= ui.add(
            egui::DragValue::new(&mut env.specular_intensity)
                .speed(0.01)
                .range(0.0..=100.0)
                .prefix("specular "),
        );
        r |= yaw_edit(ui, &mut env.rotation);
        Some(r)
    });

    typed_section::<DirectionalShadow>(ui, world, registry, history, e, id, "Directional shadow", |ui, sh| {
        let sizes = [(512u32, "512"), (1024, "1024"), (2048, "2048"), (4096, "4096")];
        let mut r = choice_edit(ui, "map size", &mut sh.map_size, &sizes);
        r |= choice_edit(ui, "filter", &mut sh.filter, &SHADOW_FILTERS);
        r |= ui.add(egui::Slider::new(&mut sh.strength, 0.0..=1.0).text("strength"));
        r |= ui.add(
            egui::DragValue::new(&mut sh.depth_bias)
                .speed(0.001)
                .range(0.0..=10.0)
                .prefix("depth bias "),
        );
        r |= ui.add(
            egui::DragValue::new(&mut sh.normal_bias)
                .speed(0.01)
                .range(0.0..=10.0)
                .prefix("normal bias "),
        );
        r |= ui.add(
            egui::DragValue::new(&mut sh.slope_bias)
                .speed(0.01)
                .range(0.0..=10.0)
                .prefix("slope bias "),
        );
        r |= ui.add(
            egui::DragValue::new(&mut sh.caster_depth)
                .speed(1.0)
                .range(0.0..=1e5)
                .prefix("caster depth "),
        );
        Some(r)
    });

    typed_section::<Visibility>(ui, world, registry, history, e, id, "Visibility", |ui, v| {
        let mut visible = *v == Visibility::Visible;
        let r = ui.checkbox(&mut visible, "visible");
        *v = if visible {
            Visibility::Visible
        } else {
            Visibility::Hidden
        };
        Some(r)
    });

    let mesh_changed = typed_section::<MeshSource>(ui, world, registry, history, e, id, "Mesh source", |ui, s| {
        ui.label(format!("kind: {}", s.label()));
        let r = match s {
            MeshSource::Cube { size } | MeshSource::Plane { size } => ui.add(
                egui::DragValue::new(size)
                    .speed(0.01)
                    .range(0.001..=1e4)
                    .prefix("size "),
            ),
            MeshSource::Sphere {
                radius,
                sectors,
                stacks,
            } => {
                let mut r = ui.add(
                    egui::DragValue::new(radius)
                        .speed(0.01)
                        .range(0.001..=1e4)
                        .prefix("radius "),
                );
                r |= ui.add(egui::DragValue::new(sectors).range(3..=256).prefix("sectors "));
                r |= ui.add(egui::DragValue::new(stacks).range(2..=256).prefix("stacks "));
                r
            }
            MeshSource::Data(m) => ui.label(format!(
                "{} vertices, {} triangles",
                m.vertex_count(),
                m.triangle_count()
            )),
        };
        Some(r)
    });

    if let Ok(mesh) = world.get::<&Mesh3d>(e).map(|m| *m) {
        egui::CollapsingHeader::new("Mesh")
            .id_salt(("space3d_inspector", "Mesh3d"))
            .default_open(false)
            .show(ui, |ui| {
                ui.label(format!("handle #{}", mesh.0.index()));
                if let Some(a) = assets {
                    if let Some(t) = a.mesh_triangles(mesh.0) {
                        ui.label(format!("{t} triangles"));
                    }
                    if let Some(b) = a.mesh_aabb(mesh.0) {
                        ui.label(format!("bounds {:.2} .. {:.2}", b.min, b.max));
                    }
                }
            });
    }

    json_sections(ui, world, registry, history, state, e, id);
    add_component_ui(ui, world, registry, history, state, e, id);
    mesh_changed
}

fn json_sections(
    ui: &mut egui::Ui,
    world: &mut World,
    registry: &ComponentRegistry,
    history: &mut EditorHistory,
    state: &mut InspectorState,
    e: Entity,
    id: StableId,
) {
    let values: BTreeMap<String, ComponentValue> = component_values(world, registry, e).unwrap_or_default();
    // Components being added have no value yet but an open editor.
    let mut names_shown: Vec<String> = values
        .keys()
        .filter(|n| !TYPED.contains(&n.as_str()))
        .cloned()
        .collect();
    for n in state.json_edits.keys() {
        if !names_shown.contains(n) && !values.contains_key(n) {
            names_shown.push(n.clone());
        }
    }
    for name in names_shown {
        let current = values.get(&name).cloned();
        egui::CollapsingHeader::new(&name)
            .id_salt(("space3d_inspector_json", &name))
            .default_open(true)
            .show(ui, |ui| {
                if let Some(text) = state.json_edits.get_mut(&name) {
                    ui.add(
                        egui::TextEdit::multiline(text)
                            .code_editor()
                            .desired_rows(4)
                            .desired_width(f32::INFINITY),
                    );
                    let (mut apply, mut cancel) = (false, false);
                    ui.horizontal(|ui| {
                        apply = ui.button("Apply").clicked();
                        cancel = ui.button("Cancel").clicked();
                    });
                    if apply {
                        let result = serde_json::from_str::<ComponentValue>(text)
                            .map_err(|err| err.to_string())
                            .and_then(|v| set_component_value(world, registry, e, &name, v.clone()).map(|_| v));
                        match result {
                            Ok(v) => {
                                history.push(EditCommand::set_json(
                                    format!("Edit {name}"),
                                    id,
                                    name.clone(),
                                    current.clone(),
                                    Some(v),
                                ));
                                state.json_edits.remove(&name);
                                state.json_errors.remove(&name);
                            }
                            Err(err) => {
                                state.json_errors.insert(name.clone(), err);
                            }
                        }
                    }
                    if cancel {
                        state.json_edits.remove(&name);
                        state.json_errors.remove(&name);
                    }
                    if let Some(err) = state.json_errors.get(&name) {
                        ui.colored_label(ui.visuals().error_fg_color, err);
                    }
                } else if let Some(v) = &current {
                    let pretty = serde_json::to_string_pretty(v).unwrap_or_default();
                    ui.label(egui::RichText::new(&pretty).monospace());
                    ui.horizontal(|ui| {
                        if ui.small_button("Edit JSON").clicked() {
                            state.json_edits.insert(name.clone(), pretty.clone());
                        }
                        if can_remove(&name) && ui.small_button("Remove").clicked() {
                            remove_component(world, e, &name);
                            history.push(EditCommand::set_json(
                                format!("Remove {name}"),
                                id,
                                name.clone(),
                                current.clone(),
                                None,
                            ));
                        }
                    });
                }
            });
    }
}

/// Default value for a known component name.
fn insert_default(world: &mut World, e: Entity, name: &str) -> bool {
    let r = match name {
        "Name" => world.insert_one(e, Name("Entity".into())),
        space3d_scene::TRANSFORM => world.insert_one(e, Transform::IDENTITY),
        names::MATERIAL => world.insert_one(e, Material::default()),
        names::DIRECTIONAL_LIGHT => world.insert_one(e, DirectionalLight::default()),
        names::POINT_LIGHT => world.insert_one(e, PointLight::default()),
        names::CAMERA => world.insert_one(e, Camera3d::default()),
        names::VISIBILITY => world.insert_one(e, Visibility::Visible),
        names::MESH_SOURCE => world.insert_one(e, MeshSource::Cube { size: 1.0 }),
        names::CAMERA_FRAME => world.insert_one(e, CameraFrame::default()),
        names::POST_PROCESS => world.insert_one(e, PostProcess::default()),
        names::DIRECTIONAL_SHADOW => world.insert_one(e, DirectionalShadow::default()),
        _ => return false,
    };
    r.is_ok()
}

fn add_component_ui(
    ui: &mut egui::Ui,
    world: &mut World,
    registry: &ComponentRegistry,
    history: &mut EditorHistory,
    state: &mut InspectorState,
    e: Entity,
    id: StableId,
) {
    let present = component_values(world, registry, e).unwrap_or_default();
    let options: Vec<String> = registry
        .names()
        .filter(|n| !HIDDEN.contains(n) && !present.contains_key(*n) && *n != space3d_scene::PARENT)
        .map(str::to_owned)
        .collect();
    if options.is_empty() {
        return;
    }
    ui.separator();
    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt("space3d_add_component")
            .selected_text(if state.add_pick.is_empty() {
                "Add component..."
            } else {
                &state.add_pick
            })
            .show_ui(ui, |ui| {
                for o in &options {
                    ui.selectable_value(&mut state.add_pick, o.clone(), o);
                }
            });
        if ui
            .add_enabled(!state.add_pick.is_empty(), egui::Button::new("Add"))
            .clicked()
        {
            let name = std::mem::take(&mut state.add_pick);
            if insert_default(world, e, &name) {
                let after = component_values(world, registry, e).and_then(|v| v.get(&name).cloned());
                history.push(EditCommand::set_json(format!("Add {name}"), id, name, None, after));
            } else {
                // Unknown type: let the user write its JSON value.
                state.json_edits.insert(name, "{}".into());
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_for_known_components() {
        let mut w = World::new();
        let e = w.spawn(());
        for n in TYPED {
            if n != space3d_scene::PARENT {
                assert!(insert_default(&mut w, e, n), "{n}");
            }
        }
        assert!(!insert_default(&mut w, e, "Custom"));
        assert!(w.satisfies::<(&Camera3d, &MeshSource, &Visibility)>(e));
        assert!(w.satisfies::<(&CameraFrame, &PostProcess, &DirectionalShadow)>(e));
    }
}
