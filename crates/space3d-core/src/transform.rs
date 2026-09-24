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

/// Propagates `Transform` → `GlobalTransform` (supports one-level-deep and
/// deeper hierarchies via iterative resolution).
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
    // Children: resolve until fixed point (depth-bounded).
    let children: Vec<(hecs::Entity, hecs::Entity, Mat4)> = world
        .query::<(hecs::Entity, &Parent, &Transform)>()
        .iter()
        .map(|(e, p, t)| (e, p.0, t.matrix()))
        .collect();
    for _depth in 0..32 {
        let mut changed = false;
        for &(e, parent, local) in &children {
            let Ok(pg) = world.get::<&GlobalTransform>(parent).map(|g| g.0) else {
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
        for (e, g) in missing.drain(..) {
            let _ = world.insert_one(e, g);
            changed = true;
        }
        if !changed {
            break;
        }
    }
}
