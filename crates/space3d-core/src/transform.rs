use glam::{Mat4, Quat, Vec3};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Transform {
    pub translation: Vec3,
    pub rotation: Quat,
    pub scale: Vec3,
}

impl Default for Transform {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl Transform {
    pub const IDENTITY: Self = Self {
        translation: Vec3::ZERO,
        rotation: Quat::IDENTITY,
        scale: Vec3::ONE,
    };

    pub fn from_xyz(x: f32, y: f32, z: f32) -> Self {
        Self {
            translation: Vec3::new(x, y, z),
            ..Self::IDENTITY
        }
    }
    pub fn with_rotation(mut self, r: Quat) -> Self {
        self.rotation = r;
        self
    }
    pub fn with_scale(mut self, s: Vec3) -> Self {
        self.scale = s;
        self
    }
    pub fn looking_at(mut self, target: Vec3, up: Vec3) -> Self {
        let m = glam::camera::rh::view::look_at_mat4(self.translation, target, up).inverse();
        self.rotation = Quat::from_mat4(&m);
        self
    }
    pub fn matrix(&self) -> Mat4 {
        Mat4::from_scale_rotation_translation(self.scale, self.rotation, self.translation)
    }
    pub fn forward(&self) -> Vec3 {
        self.rotation * Vec3::NEG_Z
    }
}

/// World-space matrix, written by the core `PostUpdate` transform system.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GlobalTransform(pub Mat4);

/// Optional parent link; child `GlobalTransform = parent.Global * child.Transform`.
#[derive(Clone, Copy, Debug)]
pub struct Parent(pub hecs::Entity);

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Name(pub String);

/// A child of the hierarchy: entity, parent, local matrix, and whether the parent is itself a
/// child (only those can need more than one pass).
type ChildEntry = (hecs::Entity, hecs::Entity, Mat4, bool);

thread_local! {
    /// Scratch of [`propagate_transforms`], kept between calls so a steady-state frame with
    /// parented entities allocates nothing.
    static CHILDREN: std::cell::Cell<Vec<ChildEntry>> = const { std::cell::Cell::new(Vec::new()) };
}

/// Propagates `Transform` → `GlobalTransform` (supports one-level-deep and
/// deeper hierarchies via iterative resolution). Allocates only while entities gain their
/// `GlobalTransform` or the hierarchy outgrows its largest size so far.
pub fn propagate_transforms(world: &mut hecs::World) {
    // Roots first.
    let mut missing = Vec::new();
    for (e, t, g) in world
        .query_mut::<(hecs::Entity, &Transform, Option<&mut GlobalTransform>)>()
        .without::<&Parent>()
    {
        match g {
            Some(g) => g.0 = t.matrix(),
            None => missing.push((e, GlobalTransform(t.matrix()))),
        }
    }
    for (e, g) in missing.drain(..) {
        let _ = world.insert_one(e, g);
    }
    // Children. One pass settles every child of a root (the common case: the parts of a
    // model under its root), the parent's matrix kept from one sibling to the next. Only children of children are resolved again, until nothing
    // changes (depth-bounded).
    let mut children = CHILDREN.take();
    children.clear();
    {
        let mut last: Option<(hecs::Entity, bool)> = None;
        children.extend(
            world
                .query::<(hecs::Entity, &Parent, &Transform)>()
                .iter()
                .map(|(e, p, t)| {
                    let nested = match last {
                        Some((parent, nested)) if parent == p.0 => nested,
                        _ => {
                            let nested = world.get::<&Parent>(p.0).is_ok();
                            last = Some((p.0, nested));
                            nested
                        }
                    };
                    (e, p.0, t.matrix(), nested)
                }),
        );
    }
    let any_nested = children.iter().any(|c| c.3);
    for depth in 0..32 {
        let mut changed = false;
        {
            let mut last: Option<(hecs::Entity, Option<Mat4>)> = None;
            for &(e, parent, local, nested) in &children {
                if depth > 0 && !nested {
                    continue;
                }
                let pg = match last {
                    Some((p, pg)) if p == parent => pg,
                    _ => {
                        let pg = world.get::<&GlobalTransform>(parent).ok().map(|g| g.0);
                        last = Some((parent, pg));
                        pg
                    }
                };
                let Some(pg) = pg else {
                    continue;
                };
                let new = pg * local;
                match world.get::<&mut GlobalTransform>(e) {
                    Ok(mut g) => {
                        if g.0 != new {
                            g.0 = new;
                            changed = true;
                        }
                    }
                    Err(_) => missing.push((e, GlobalTransform(new))),
                }
            }
        }
        for (e, g) in missing.drain(..) {
            let _ = world.insert_one(e, g);
            changed = true;
        }
        if !changed || !any_nested {
            break;
        }
    }
    CHILDREN.set(children);
}
