//! Viewport interaction: ray picking against mesh AABBs and a translate gizmo ("gizmo-lite")
//! drawn with egui over the 3D view.

use crate::components::{EditorInternal, ensure_stable_id};
use crate::history::{EditCommand, EditorHistory};
use space3d_core::glam::{Mat4, Vec2, Vec3};
use space3d_core::hecs::{Entity, World};
use space3d_core::{GlobalTransform, Parent, Transform};
use space3d_render::{Camera3d, DirectionalLight, Mesh3d, PointLight, RenderAssets, RenderTarget, Visibility};
use space3d_scene::StableId;

/// View + projection of the camera the viewport shows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewCamera {
    /// World → view.
    pub view: Mat4,
    /// View → clip (wgpu depth 0..1).
    pub proj: Mat4,
    /// Camera position.
    pub eye: Vec3,
}

impl ViewCamera {
    /// World → clip.
    pub fn view_proj(&self) -> Mat4 {
        self.proj * self.view
    }

    /// Project a world point to screen coordinates in a `size` screen (y down). `None` behind
    /// the camera.
    pub fn project(&self, p: Vec3, size: Vec2) -> Option<Vec2> {
        let clip = self.view_proj() * p.extend(1.0);
        if clip.w <= 1e-5 {
            return None;
        }
        let n = clip.truncate() / clip.w;
        Some(Vec2::new((n.x * 0.5 + 0.5) * size.x, (0.5 - n.y * 0.5) * size.y))
    }

    /// World-space ray through screen position `pos` of a `size` screen.
    pub fn ray(&self, pos: Vec2, size: Vec2) -> (Vec3, Vec3) {
        let ndc = Vec2::new(pos.x / size.x * 2.0 - 1.0, 1.0 - pos.y / size.y * 2.0);
        let inv = self.view_proj().inverse();
        let near = inv.project_point3(ndc.extend(0.0));
        let far = inv.project_point3(ndc.extend(1.0));
        (near, (far - near).normalize_or_zero())
    }
}

/// The active primary-target camera with the lowest order (editor-internal cameras ignored).
pub fn primary_camera(world: &World, aspect: f32) -> Option<ViewCamera> {
    world
        .query::<(&Camera3d, &GlobalTransform)>()
        .without::<&EditorInternal>()
        .iter()
        .filter(|(c, _)| c.active && c.target == RenderTarget::Primary)
        .min_by_key(|(c, _)| c.order)
        .map(|(c, g)| ViewCamera {
            view: g.0.inverse(),
            proj: c.projection.matrix(aspect),
            eye: g.0.w_axis.truncate(),
        })
}

/// Slab test; distance along `dir` to the box, if hit.
pub fn ray_aabb(origin: Vec3, dir: Vec3, min: Vec3, max: Vec3) -> Option<f32> {
    let inv = dir.recip();
    let t0 = (min - origin) * inv;
    let t1 = (max - origin) * inv;
    let tmin = t0.min(t1).max_element();
    let tmax = t0.max(t1).min_element();
    (tmax >= tmin.max(0.0)).then_some(tmin.max(0.0))
}

/// Nearest entity under the ray: meshes by their transformed AABB, lights and cameras by a
/// small sphere around their position.
pub fn pick(world: &World, assets: Option<&RenderAssets>, origin: Vec3, dir: Vec3) -> Option<(Entity, f32)> {
    let mut best: Option<(Entity, f32)> = None;
    let mut consider = |e: Entity, d: f32| {
        if best.is_none_or(|(_, b)| d < b) {
            best = Some((e, d));
        }
    };
    if let Some(assets) = assets {
        for (e, g, mesh, vis) in world
            .query::<(Entity, &GlobalTransform, &Mesh3d, Option<&Visibility>)>()
            .without::<&EditorInternal>()
            .iter()
        {
            if vis == Some(&Visibility::Hidden) {
                continue;
            }
            let Some(aabb) = assets.mesh_aabb(mesh.0) else { continue };
            let inv = g.0.inverse();
            let lo = inv.transform_point3(origin);
            let ld = inv.transform_vector3(dir);
            if let Some(t) = ray_aabb(lo, ld, aabb.min, aabb.max) {
                let hit = g.0.transform_point3(lo + ld * t);
                consider(e, (hit - origin).length());
            }
        }
    }
    for (e, g, _, _, _) in world
        .query::<(
            Entity,
            &GlobalTransform,
            Option<&PointLight>,
            Option<&DirectionalLight>,
            Option<&Camera3d>,
        )>()
        .without::<&EditorInternal>()
        .without::<&Mesh3d>()
        .iter()
        .filter(|(_, _, p, d, c)| p.is_some() || d.is_some() || c.is_some())
    {
        let center = g.0.w_axis.truncate();
        let to = center - origin;
        let along = to.dot(dir);
        if along > 0.0 && (to - dir * along).length() < 0.3 {
            consider(e, along);
        }
    }
    best
}

#[derive(Clone, Copy, Debug)]
struct GizmoDrag {
    axis: usize,
    start: Transform,
    press: Vec2,
    id: StableId,
    entity: Entity,
}

/// Translate-gizmo state.
#[derive(Clone, Debug, Default)]
pub struct Gizmo {
    drag: Option<GizmoDrag>,
}

const AXIS_COLORS: [egui::Color32; 3] = [
    egui::Color32::from_rgb(230, 70, 70),
    egui::Color32::from_rgb(90, 210, 90),
    egui::Color32::from_rgb(80, 130, 240),
];

fn dist_to_segment(p: Vec2, a: Vec2, b: Vec2) -> f32 {
    let ab = b - a;
    let t = ((p - a).dot(ab) / ab.length_squared().max(1e-6)).clamp(0.0, 1.0);
    (a + ab * t - p).length()
}

/// Result of one viewport interaction frame.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ViewportOutcome {
    /// New selection from a click (`Some(None)` = clicked empty space).
    pub select: Option<Option<Entity>>,
    /// A gizmo drag is in progress.
    pub dragging: bool,
}

impl Gizmo {
    /// Whether a drag is in progress.
    pub fn is_dragging(&self) -> bool {
        self.drag.is_some()
    }

    /// Draw the gizmo for `selected` and handle clicks/drags inside `viewport` (points).
    #[allow(clippy::too_many_arguments)]
    pub fn interact(
        &mut self,
        ctx: &egui::Context,
        viewport: egui::Rect,
        world: &mut World,
        assets: Option<&RenderAssets>,
        history: &mut EditorHistory,
        selected: Option<Entity>,
        aspect: f32,
    ) -> ViewportOutcome {
        let mut out = ViewportOutcome::default();
        let screen = ctx.content_rect();
        let size = Vec2::new(screen.width(), screen.height());
        let Some(cam) = primary_camera(world, aspect) else {
            self.drag = None;
            return out;
        };
        let (pointer, pressed, down) = ctx.input(|i| {
            (
                i.pointer.hover_pos().map(|p| Vec2::new(p.x, p.y)),
                i.pointer.primary_pressed(),
                i.pointer.primary_down(),
            )
        });
        let in_view = pointer.is_some_and(|p| {
            let pos = egui::pos2(p.x, p.y);
            viewport.contains(pos) && ctx.layer_id_at(pos).is_none_or(|l| l.order == egui::Order::Background)
        });

        // Gizmo geometry for the selection.
        let gizmo = selected
            .filter(|&e| world.contains(e) && !world.satisfies::<&EditorInternal>(e))
            .and_then(|e| {
                let g = world.get::<&GlobalTransform>(e).ok().map(|g| g.0)?;
                let origin = g.w_axis.truncate();
                let o = cam.project(origin, size)?;
                let len = (origin - cam.eye).length() * 0.15;
                let ends = [Vec3::X, Vec3::Y, Vec3::Z].map(|a| cam.project(origin + a * len, size));
                Some((e, origin, o, len, ends))
            });
        if let Some((_, _, o, _, ends)) = &gizmo {
            let painter = ctx
                .layer_painter(egui::LayerId::new(
                    egui::Order::Background,
                    egui::Id::new("space3d_gizmo"),
                ))
                .with_clip_rect(viewport);
            let to_pos = |v: Vec2| egui::pos2(v.x, v.y);
            for (i, end) in ends.iter().enumerate() {
                if let Some(end) = end {
                    let active = self.drag.is_some_and(|d| d.axis == i);
                    let width = if active { 4.0 } else { 2.5 };
                    painter.line_segment([to_pos(*o), to_pos(*end)], egui::Stroke::new(width, AXIS_COLORS[i]));
                    painter.circle_filled(to_pos(*end), 5.0, AXIS_COLORS[i]);
                }
            }
            painter.circle_filled(to_pos(*o), 3.5, egui::Color32::WHITE);
        }

        // Continue / finish a drag.
        if let Some(d) = self.drag {
            if !world.contains(d.entity) {
                self.drag = None;
                return out;
            }
            out.dragging = true;
            let current = world.get::<&Transform>(d.entity).map(|t| *t).unwrap_or(d.start);
            if down {
                if let (Some(p), Some((_, _, o, len, ends))) = (pointer, &gizmo)
                    && let Some(end) = ends[d.axis]
                {
                    let axis_screen = end - *o;
                    let axis_len = axis_screen.length().max(1.0);
                    let amount = (p - d.press).dot(axis_screen / axis_len) / axis_len * *len;
                    let axis_world = [Vec3::X, Vec3::Y, Vec3::Z][d.axis] * amount;
                    let parent_inv = world
                        .get::<&Parent>(d.entity)
                        .ok()
                        .and_then(|p| world.get::<&GlobalTransform>(p.0).ok().map(|g| g.0.inverse()));
                    let local = parent_inv.map_or(axis_world, |m| m.transform_vector3(axis_world));
                    let mut t = d.start;
                    t.translation += local;
                    if t != current {
                        let _ = world.insert_one(d.entity, t);
                    }
                }
            } else {
                if current != d.start {
                    history.push(EditCommand::set("Move", d.id, Some(d.start), Some(current)));
                }
                self.drag = None;
            }
            return out;
        }

        if !(pressed && in_view) {
            return out;
        }
        let p = pointer.expect("in_view implies a pointer");
        // Start a gizmo drag?
        if let Some((e, _, o, _, ends)) = &gizmo {
            let hit = ends
                .iter()
                .enumerate()
                .filter_map(|(i, end)| end.map(|end| (i, dist_to_segment(p, *o, end))))
                .filter(|(_, d)| *d < 8.0)
                .min_by(|a, b| a.1.total_cmp(&b.1));
            if let Some((axis, _)) = hit {
                let start = world.get::<&Transform>(*e).map(|t| *t).unwrap_or_default();
                if let Some(id) = ensure_stable_id(world, *e) {
                    self.drag = Some(GizmoDrag {
                        axis,
                        start,
                        press: p,
                        id,
                        entity: *e,
                    });
                    out.dragging = true;
                    return out;
                }
            }
        }
        // Otherwise pick.
        let (origin, dir) = cam.ray(p, size);
        out.select = Some(pick(world, assets, origin, dir).map(|(e, _)| e));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use space3d_core::{App, MeshData};

    #[test]
    fn ray_box_and_picking() {
        assert_eq!(
            ray_aabb(Vec3::new(0.0, 0.0, 5.0), Vec3::NEG_Z, Vec3::splat(-1.0), Vec3::ONE),
            Some(4.0)
        );
        assert_eq!(
            ray_aabb(Vec3::new(3.0, 0.0, 5.0), Vec3::NEG_Z, Vec3::splat(-1.0), Vec3::ONE),
            None
        );

        let mut app = App::new();
        let mut assets = RenderAssets::default();
        let cube = assets.add_mesh(MeshData::cube(1.0));
        let near = app.world.spawn((Transform::from_xyz(0.0, 0.0, 1.0), Mesh3d(cube)));
        let far = app.world.spawn((Transform::from_xyz(0.0, 0.0, -3.0), Mesh3d(cube)));
        let _hidden = app
            .world
            .spawn((Transform::from_xyz(0.0, 0.0, 3.0), Mesh3d(cube), Visibility::Hidden));
        let light = app
            .world
            .spawn((Transform::from_xyz(4.0, 0.0, 0.0), PointLight::default()));
        app.world
            .spawn((Transform::from_xyz(0.0, 0.0, 10.0), Camera3d::default()));
        app.step_ticks(1);
        let cam = primary_camera(&app.world, 1.0).unwrap();
        let (o, d) = cam.ray(Vec2::new(50.0, 50.0), Vec2::new(100.0, 100.0));
        assert!((d - Vec3::NEG_Z).length() < 1e-4);
        assert_eq!(pick(&app.world, Some(&assets), o, d).map(|p| p.0), Some(near));
        let _ = far;
        let (o, d) = (Vec3::new(4.0, 0.0, 10.0), Vec3::NEG_Z);
        assert_eq!(pick(&app.world, Some(&assets), o, d).map(|p| p.0), Some(light));
        let center = cam.project(Vec3::ZERO, Vec2::new(100.0, 100.0)).unwrap();
        assert!((center - Vec2::new(50.0, 50.0)).length() < 1e-3);
    }
}
