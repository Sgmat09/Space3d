//! Space3d physics: [rapier3d](https://rapier.rs) rigid bodies, colliders and raycasts,
//! stepped deterministically on the fixed tick.
//!
//! Add [`PhysicsPlugin`], then spawn entities with a `Transform` plus [`RigidBody`] and/or
//! [`Collider`]. Every `Stage::FixedUpdate` tick the plugin:
//!
//! 1. removes rapier bodies of despawned entities (or entities that lost their physics components),
//! 2. creates bodies for new entities and pushes game-side edits (teleports, [`Velocity`],
//!    [`Restitution`]/[`Friction`], [`Sensor`], shape changes, kinematic targets),
//! 3. steps rapier by exactly `Time::fixed_delta`,
//! 4. writes dynamic body poses back into `Transform` (and [`Velocity`] if present),
//! 5. emits [`CollisionEvent`]s as `Events<CollisionEvent>`.
//!
//! Because stepping only happens on fixed ticks, the simulation result depends on the
//! number of ticks, never on the frame-time pattern.
//!
//! Physics treats an entity's `Transform` as its world pose, so physics bodies should be
//! root entities (no `Parent`).
//!
//! ```
//! use space3d_core::prelude::*;
//! use space3d_physics::*;
//!
//! let mut app = App::new();
//! app.add_plugin(PhysicsPlugin::default());
//! app.world.spawn((Transform::from_xyz(0.0, -0.5, 0.0), RigidBody::Fixed, Collider::cuboid(Vec3::new(20.0, 1.0, 20.0))));
//! let ball = app.world.spawn((Transform::from_xyz(0.0, 3.0, 0.0), RigidBody::Dynamic, Collider::ball(0.5)));
//! app.step_ticks(180);
//! let y = app.world.get::<&Transform>(ball).unwrap().translation.y;
//! assert!((y - 0.5).abs() < 0.05);
//! ```
#![warn(missing_docs)]

mod components;
mod world;

pub use components::{Collider, Friction, GravityScale, Restitution, RigidBody, Sensor, Velocity};
pub use rapier3d;
pub use world::{CollisionEvent, Physics, RayFilter, RayHit};

use space3d_core::glam::Vec3;
use space3d_core::{App, Events, Plugin, Stage, SystemCtx};

/// Adds the [`Physics`] resource, `Events<CollisionEvent>` and the fixed-tick step system.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PhysicsPlugin {
    /// Initial gravity (m/s²). Change later with [`Physics::set_gravity`].
    pub gravity: Vec3,
}

impl Default for PhysicsPlugin {
    fn default() -> Self {
        Self {
            gravity: Vec3::new(0.0, -9.81, 0.0),
        }
    }
}

impl Plugin for PhysicsPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(Physics::new(self.gravity));
        app.add_event::<CollisionEvent>();
        app.add_system_named(Stage::FixedUpdate, "physics::step", physics_step);
    }
}

/// The fixed-tick system installed by [`PhysicsPlugin`]. Public so custom schedules
/// (e.g. rollback resimulation) can run it directly.
pub fn physics_step(ctx: &mut SystemCtx) {
    let dt = ctx.time().fixed_delta_secs();
    let SystemCtx { world, resources } = ctx;
    let events = {
        let physics = resources.expect_mut::<Physics>();
        physics.sync_from_world(world);
        physics.step(dt);
        physics.write_back(world);
        physics.drain_events()
    };
    if !events.is_empty() {
        let queue = resources.init::<Events<CollisionEvent>>();
        for e in events {
            queue.send(e);
        }
    }
}
