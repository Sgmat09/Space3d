//! ECS components understood by [`crate::PhysicsPlugin`].
use space3d_core::MeshData;
use space3d_core::glam::Vec3;

/// How a body participates in the simulation.
///
/// An entity with a [`Collider`] but no `RigidBody` is treated as [`RigidBody::Fixed`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum RigidBody {
    /// Moved by forces, gravity and contacts. Physics writes its pose back into `Transform`.
    #[default]
    Dynamic,
    /// Never moves on its own (ground, walls). Editing its `Transform` teleports it.
    Fixed,
    /// Driven by the game through `Transform`; pushes dynamic bodies but is not pushed back.
    KinematicPosition,
}

/// Collision shape, expressed in the entity's local space.
///
/// The entity's `Transform::scale` is baked into the shape when the collider is built
/// (and whenever the shape or scale changes).
#[derive(Clone, Debug, PartialEq)]
pub enum Collider {
    /// Box given by its half extents.
    Cuboid {
        /// Half size along each local axis.
        half_extents: Vec3,
    },
    /// Sphere.
    Ball {
        /// Sphere radius.
        radius: f32,
    },
    /// Capsule aligned with the local Y axis.
    Capsule {
        /// Half the length of the cylindrical segment (excluding the caps).
        half_height: f32,
        /// Radius of the capsule.
        radius: f32,
    },
    /// Static triangle mesh (use on `Fixed`/`KinematicPosition` bodies; it has no volume/mass).
    TriMesh(MeshData),
}

impl Collider {
    /// Box from full size (`width`, `height`, `depth`).
    pub fn cuboid(size: Vec3) -> Self {
        Self::Cuboid {
            half_extents: size * 0.5,
        }
    }

    /// Sphere with the given radius.
    pub fn ball(radius: f32) -> Self {
        Self::Ball { radius }
    }

    /// Y-aligned capsule.
    pub fn capsule(half_height: f32, radius: f32) -> Self {
        Self::Capsule { half_height, radius }
    }

    /// Triangle mesh collider built from mesh positions/indices.
    pub fn trimesh(mesh: MeshData) -> Self {
        Self::TriMesh(mesh)
    }

    /// Cheap fingerprint used to detect shape edits without comparing whole meshes.
    pub(crate) fn key(&self, scale: Vec3) -> ShapeKey {
        let s = scale.to_array().map(f32::to_bits);
        match self {
            Collider::Cuboid { half_extents } => ShapeKey::Cuboid(half_extents.to_array().map(f32::to_bits), s),
            Collider::Ball { radius } => ShapeKey::Ball(radius.to_bits(), s),
            Collider::Capsule { half_height, radius } => ShapeKey::Capsule(half_height.to_bits(), radius.to_bits(), s),
            Collider::TriMesh(m) => ShapeKey::TriMesh(m.positions.len(), m.indices.len(), s),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShapeKey {
    Cuboid([u32; 3], [u32; 3]),
    Ball(u32, [u32; 3]),
    Capsule(u32, u32, [u32; 3]),
    TriMesh(usize, usize, [u32; 3]),
}

/// Linear and angular velocity (world space, angular = axis * radians/s).
///
/// Read it to observe a dynamic body; write it to set the body's velocity
/// (the change is applied before the next fixed tick).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Velocity {
    /// Metres per second.
    pub linear: Vec3,
    /// Scaled axis, radians per second.
    pub angular: Vec3,
}

impl Velocity {
    /// Velocity with only a linear component.
    pub fn linear(linear: Vec3) -> Self {
        Self {
            linear,
            angular: Vec3::ZERO,
        }
    }
}

/// Bounciness in `0..=1` (default 0). Combined with the other collider by averaging.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Restitution(pub f32);

impl Default for Restitution {
    fn default() -> Self {
        Self(0.0)
    }
}

/// Coulomb friction coefficient (default 0.5). Combined with the other collider by averaging.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Friction(pub f32);

impl Default for Friction {
    fn default() -> Self {
        Self(0.5)
    }
}

/// Marks the collider as a trigger volume: it reports [`crate::CollisionEvent`]s but
/// generates no contact forces.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sensor;

/// Multiplier applied to the global gravity for one dynamic body (default 1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GravityScale(pub f32);

impl Default for GravityScale {
    fn default() -> Self {
        Self(1.0)
    }
}
