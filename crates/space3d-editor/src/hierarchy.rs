//! Scene hierarchy panel: `Name` / `Parent` tree with selection, drag-and-drop reparenting and
//! context menus.

use crate::components::{EditorInternal, display_name};
use crate::scene_ops::{AddKind, subtree};
use space3d_core::Parent;
use space3d_core::hecs::{Entity, World};
use std::collections::HashMap;

/// What the user did in the hierarchy.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HierarchyAction {
    /// Select an entity.
    Select(Entity),
    /// Change the parent (`None` = make root).
    Reparent {
        /// Entity to move.
        child: Entity,
        /// New parent.
        parent: Option<Entity>,
    },
    /// Delete (with descendants).
    Delete(Entity),
    /// Duplicate (with descendants).
    Duplicate(Entity),
    /// Add a new entity, optionally as a child.
    Add(AddKind, Option<Entity>),
}

struct Tree {
    names: HashMap<Entity, String>,
    children: HashMap<Entity, Vec<Entity>>,
    roots: Vec<Entity>,
}

fn build_tree(world: &World) -> Tree {
    let mut entities: Vec<Entity> = world
        .iter()
        .map(|e| e.entity())
        .filter(|&e| !world.satisfies::<&EditorInternal>(e))
        .collect();
    entities.sort_unstable_by_key(|e| (e.id(), e.to_bits()));
    let names = entities
        .iter()
        .map(|&e| (e, display_name(world, e)))
        .collect::<HashMap<_, _>>();
    let mut children: HashMap<Entity, Vec<Entity>> = HashMap::new();
    let mut roots = Vec::new();
    for &e in &entities {
        match world.get::<&Parent>(e).map(|p| p.0) {
            Ok(p) if names.contains_key(&p) && p != e => children.entry(p).or_default().push(e),
            _ => roots.push(e),
        }
    }
    Tree { names, children, roots }
}

/// Whether moving `child` under `parent` keeps the hierarchy acyclic.
pub fn can_reparent(world: &World, child: Entity, parent: Option<Entity>) -> bool {
    match parent {
        None => true,
        Some(p) => p != child && !subtree(world, child).contains(&p),
    }
}

/// Draw the hierarchy; returns the requested actions.
pub fn hierarchy_ui(
    ui: &mut egui::Ui,
    world: &World,
    selected: Option<Entity>,
    filter: &mut String,
) -> Vec<HierarchyAction> {
    let mut actions = Vec::new();
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(filter)
                .hint_text("search")
                .desired_width(ui.available_width() - 30.0),
        );
        ui.menu_button("+", |ui| {
            for kind in AddKind::ALL {
                if ui.button(kind.label()).clicked() {
                    actions.push(HierarchyAction::Add(kind, None));
                    ui.close();
                }
            }
        });
    });
    ui.separator();
    let tree = build_tree(world);
    egui::ScrollArea::vertical()
        .id_salt("space3d_hierarchy")
        .auto_shrink([false, false])
        .max_height(ui.available_height() - 36.0)
        .show(ui, |ui| {
            let filter = filter.to_lowercase();
            if filter.is_empty() {
                for &r in &tree.roots {
                    node(ui, &tree, r, selected, &mut actions);
                }
            } else {
                let mut matches: Vec<(&Entity, &String)> = tree
                    .names
                    .iter()
                    .filter(|(_, n)| n.to_lowercase().contains(&filter))
                    .collect();
                matches.sort_by_key(|(e, _)| (e.id(), e.to_bits()));
                for (&e, name) in matches {
                    entity_label(ui, e, name, selected, &mut actions);
                }
            }
        });
    // Drop zone: make root.
    let (rect, response) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 28.0), egui::Sense::hover());
    let hovering = response.dnd_hover_payload::<Entity>().is_some();
    ui.painter().rect_stroke(
        rect,
        4.0,
        egui::Stroke::new(
            1.0,
            if hovering {
                egui::Color32::LIGHT_BLUE
            } else {
                egui::Color32::from_gray(70)
            },
        ),
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        "drop here to unparent",
        egui::FontId::proportional(11.0),
        egui::Color32::GRAY,
    );
    if let Some(child) = response.dnd_release_payload::<Entity>() {
        actions.push(HierarchyAction::Reparent {
            child: *child,
            parent: None,
        });
    }
    actions
        .into_iter()
        .filter(|a| match *a {
            HierarchyAction::Reparent { child, parent } => can_reparent(world, child, parent),
            _ => true,
        })
        .collect()
}

fn node(ui: &mut egui::Ui, tree: &Tree, e: Entity, selected: Option<Entity>, actions: &mut Vec<HierarchyAction>) {
    let name = &tree.names[&e];
    match tree.children.get(&e) {
        Some(kids) => {
            let id = ui.make_persistent_id(("space3d_hierarchy_node", e.to_bits()));
            egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, true)
                .show_header(ui, |ui| entity_label(ui, e, name, selected, actions))
                .body(|ui| {
                    for &k in kids {
                        node(ui, tree, k, selected, actions);
                    }
                });
        }
        None => {
            ui.horizontal(|ui| {
                ui.add_space(ui.spacing().indent);
                entity_label(ui, e, name, selected, actions);
            });
        }
    }
}

fn entity_label(
    ui: &mut egui::Ui,
    e: Entity,
    name: &str,
    selected: Option<Entity>,
    actions: &mut Vec<HierarchyAction>,
) {
    let response = ui.add(egui::Button::selectable(selected == Some(e), name).sense(egui::Sense::click_and_drag()));
    if response.clicked() || response.drag_started() {
        actions.push(HierarchyAction::Select(e));
    }
    if response.dragged() {
        response.dnd_set_drag_payload(e);
    }
    if let Some(child) = response.dnd_release_payload::<Entity>()
        && *child != e
    {
        actions.push(HierarchyAction::Reparent {
            child: *child,
            parent: Some(e),
        });
    }
    response.context_menu(|ui| {
        ui.menu_button("Add child", |ui| {
            for kind in AddKind::ALL {
                if ui.button(kind.label()).clicked() {
                    actions.push(HierarchyAction::Add(kind, Some(e)));
                    ui.close();
                }
            }
        });
        if ui.button("Duplicate").clicked() {
            actions.push(HierarchyAction::Duplicate(e));
            ui.close();
        }
        if ui.button("Unparent").clicked() {
            actions.push(HierarchyAction::Reparent { child: e, parent: None });
            ui.close();
        }
        if ui.button("Delete").clicked() {
            actions.push(HierarchyAction::Delete(e));
            ui.close();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use space3d_core::Name;

    #[test]
    fn tree_and_cycle_checks() {
        let mut w = World::new();
        let a = w.spawn((Name("a".into()),));
        let b = w.spawn((Name("b".into()), Parent(a)));
        let c = w.spawn((Name("c".into()), Parent(b)));
        w.spawn((EditorInternal, Name("hidden".into())));
        let t = build_tree(&w);
        assert_eq!(t.roots, vec![a]);
        assert_eq!(t.children[&a], vec![b]);
        assert_eq!(t.names.len(), 3);
        assert!(!can_reparent(&w, a, Some(c)), "no cycles");
        assert!(can_reparent(&w, c, Some(a)));
        assert!(!can_reparent(&w, a, Some(a)));
    }
}
