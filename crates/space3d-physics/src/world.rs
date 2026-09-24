//! The [`Physics`] resource: owns the rapier world and keeps it in sync with the ECS.
use crate::components::{Collider, Friction, GravityScale, Restitution, RigidBody, Sensor, ShapeKey, Velocity};
use rapier3d::prelude as rp;
use space3d_core::Transform;
use space3d_core::glam::{Quat, Vec3};
use space3d_core::hecs::{Entity, Or, World};
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

/// Result of a successful [`Physics::raycast`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RayHit {
    /// Entity owning the collider that was hit.
    pub entity: Entity,
    /// World-space hit point.
    pub point: Vec3,
    /// World-space surface normal at the hit point.
    pub normal: Vec3,
    /// Distance along the (normalized) ray direction.
    pub toi: f32,
}

/// Options for [`Physics::raycast_with`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RayFilter {
    /// Also hit [`Sensor`] colliders (default: `false`).
    pub include_sensors: bool,
    /// Ignore every collider belonging to this entity (e.g. the shooter).
    pub exclude: Option<Entity>,
}

/// Contact / trigger notification, sent as `Events<CollisionEvent>` after every fixed tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CollisionEvent {
    /// `a` and `b` started touching (or overlapping, for sensors).
    Started {
        /// First entity.
        a: Entity,
        /// Second entity.
        b: Entity,
        /// At least one collider is a [`Sensor`].
        sensor: bool,
    },
    /// `a` and `b` stopped touching.
    Stopped {
        /// First entity.
        a: Entity,
        /// Second entity.
        b: Entity,
        /// At least one collider is a [`Sensor`].
        sensor: bool,
        /// Fired because one of the colliders was removed (e.g. its entity despawned).
        removed: bool,
    },
}

impl CollisionEvent {
    /// The two entities involved.
    pub fn entities(&self) -> (Entity, Entity) {
        match *self {
            Self::Started { a, b, .. } | Self::Stopped { a, b, .. } => (a, b),
        }
    }

    /// Whether `entity` is one of the two participants.
    pub fn involves(&self, entity: Entity) -> bool {
        let (a, b) = self.entities();
        a == entity || b == entity
    }

    /// `true` for [`CollisionEvent::Started`].
    pub fn is_started(&self) -> bool {
        matches!(self, Self::Started { .. })
    }
}

struct BodyEntry {
    body: rp::RigidBodyHandle,
    collider: Option<rp::ColliderHandle>,
    kind: RigidBody,
    shape: Option<ShapeKey>,
    /// Pose physics last wrote into / read from the ECS; a mismatch means the game teleported it.
    pose: (Vec3, Quat),
    /// Velocity physics last wrote into the ECS.
    vel: Option<Velocity>,
}

/// Rapier event handler that just buffers collision events.
#[derive(Default)]
struct Collector(Mutex<Vec<rp::CollisionEvent>>);

impl rp::EventHandler for Collector {
    fn handle_collision_event(
        &self,
        _bodies: &rp::RigidBodySet,
        _colliders: &rp::ColliderSet,
        event: rp::CollisionEvent,
        _pair: Option<&rp::ContactPair>,
    ) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).push(event);
    }

    fn handle_contact_force_event(
        &self,
        _dt: f32,
        _bodies: &rp::RigidBodySet,
        _colliders: &rp::ColliderSet,
        _pair: &rp::ContactPair,
        _total_force_magnitude: f32,
    ) {
    }
}

/// Physics state resource. Inserted by [`crate::PhysicsPlugin`].
///
/// Bodies are created/updated/removed automatically from ECS components every fixed tick;
/// use this resource for global settings and scene queries.
pub struct Physics {
    world: rp::PhysicsWorld,
    entries: BTreeMap<Entity, BodyEntry>,
    collider_owner: HashMap<rp::ColliderHandle, Entity>,
    /// Colliders removed since the last step; their owner mapping is kept until removal events fire.
    removed: Vec<rp::ColliderHandle>,
    collector: Collector,
    paused: bool,
    steps: u64,
}

impl Default for Physics {
    fn default() -> Self {
        Self::new(Vec3::new(0.0, -9.81, 0.0))
    }
}

fn entity_bits(e: Entity) -> u128 {
    e.to_bits().get() as u128
}

fn pose(t: &Transform) -> rp::Pose {
    rp::Pose::from_parts(t.translation, t.rotation)
}

impl Physics {
    /// Empty physics world with the given gravity.
    pub fn new(gravity: Vec3) -> Self {
        let mut world = rp::PhysicsWorld::new();
        world.gravity = gravity;
        Self {
            world,
            entries: BTreeMap::new(),
            collider_owner: HashMap::new(),
            removed: Vec::new(),
            collector: Collector::default(),
            paused: false,
            steps: 0,
        }
    }

    /// Current gravity (m/s²).
    pub fn gravity(&self) -> Vec3 {
        self.world.gravity
    }

    /// Change gravity; takes effect on the next tick.
    pub fn set_gravity(&mut self, gravity: Vec3) {
        self.world.gravity = gravity;
    }

    /// Pause/resume stepping (ECS sync still happens so edits are not lost).
    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    /// Whether stepping is paused.
    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// Number of simulation steps executed so far.
    pub fn steps(&self) -> u64 {
        self.steps
    }

    /// Number of entities currently backed by a rapier body.
    pub fn body_count(&self) -> usize {
        self.entries.len()
    }

    /// Whether `entity` currently has a rapier body.
    pub fn has_body(&self, entity: Entity) -> bool {
        self.entries.contains_key(&entity)
    }

    /// Rapier handle of the body backing `entity`.
    pub fn body_handle(&self, entity: Entity) -> Option<rp::RigidBodyHandle> {
        self.entries.get(&entity).map(|e| e.body)
    }

    /// Entity owning a rapier collider.
    pub fn collider_entity(&self, collider: rp::ColliderHandle) -> Option<Entity> {
        self.collider_owner.get(&collider).copied()
    }

    /// Read-only access to the underlying rapier world (advanced queries).
    pub fn raw(&self) -> &rp::PhysicsWorld {
        &self.world
    }

    /// Mutable access to the underlying rapier world (joints, integration parameters, ...).
    ///
    /// Bodies owned by ECS entities are still managed by the plugin; don't remove them here.
    pub fn raw_mut(&mut self) -> &mut rp::PhysicsWorld {
        &mut self.world
    }

    /// Apply an instantaneous impulse (N·s) to a dynamic body's centre of mass.
    pub fn apply_impulse(&mut self, entity: Entity, impulse: Vec3) -> bool {
        let Some(h) = self.body_handle(entity) else {
            return false;
        };
        match self.world.bodies.get_mut(h) {
            Some(b) => {
                b.apply_impulse(impulse, true);
                true
            }
            None => false,
        }
    }

    /// Cast a ray against all non-sensor colliders. `dir` need not be normalized;
    /// `max_toi` and the returned [`RayHit::toi`] are distances in world units.
    ///
    /// Colliders are only queryable after they have been through at least one fixed tick.
    pub fn raycast(&self, origin: Vec3, dir: Vec3, max_toi: f32) -> Option<RayHit> {
        self.raycast_with(origin, dir, max_toi, RayFilter::default())
    }

    /// [`Physics::raycast`] with filtering options.
    pub fn raycast_with(&self, origin: Vec3, dir: Vec3, max_toi: f32, filter: RayFilter) -> Option<RayHit> {
        let dir = dir.normalize_or_zero();
        if dir == Vec3::ZERO {
            return None;
        }
        let ray = rp::Ray::new(origin, dir);
        let mut qf = rp::QueryFilter::default();
        if !filter.include_sensors {
            qf = qf.exclude_sensors();
        }
        if let Some(h) = filter.exclude.and_then(|e| self.body_handle(e)) {
            qf = qf.exclude_rigid_body(h);
        }
        let (handle, hit) = self.world.cast_ray_and_get_normal(&ray, max_toi, true, qf)?;
        let entity = self.collider_entity(handle)?;
        Some(RayHit {
            entity,
            point: ray.point_at(hit.time_of_impact),
            normal: hit.normal,
            toi: hit.time_of_impact,
        })
    }

    fn remove_entry(&mut self, entity: Entity) {
        if let Some(entry) = self.entries.remove(&entity) {
            if let Some(c) = entry.collider {
                self.removed.push(c);
            }
            self.world.remove_body(entry.body);
        }
    }

    fn build_collider(
        entity: Entity,
        shape: &Collider,
        scale: Vec3,
        restitution: f32,
        friction: f32,
        sensor: bool,
    ) -> Option<rp::Collider> {
        let s = scale.abs();
        let builder = match shape {
            Collider::Cuboid { half_extents } => {
                let h = *half_extents * s;
                rp::ColliderBuilder::cuboid(h.x, h.y, h.z)
            }
            Collider::Ball { radius } => rp::ColliderBuilder::ball(radius * s.max_element()),
            Collider::Capsule { half_height, radius } => {
                rp::ColliderBuilder::capsule_y(half_height * s.y, radius * s.x.max(s.z))
            }
            Collider::TriMesh(mesh) => {
                let verts = mesh.positions.iter().map(|p| Vec3::from(*p) * scale).collect();
                let idx = mesh.indices.as_chunks::<3>().0.to_vec();
                match rp::ColliderBuilder::trimesh(verts, idx) {
                    Ok(b) => b,
                    Err(err) => {
                        tracing::warn!(?entity, %err, "invalid trimesh collider; skipping");
                        return None;
                    }
                }
            }
        };
        let mut builder = builder
            .restitution(restitution)
            .friction(friction)
            .sensor(sensor)
            .active_events(rp::ActiveEvents::COLLISION_EVENTS)
            .user_data(entity_bits(entity));
        if sensor {
            builder = builder.active_collision_types(rp::ActiveCollisionTypes::all());
        }
        Some(builder.build())
    }

    fn insert_collider(
        &mut self,
        entity: Entity,
        body: rp::RigidBodyHandle,
        collider: rp::Collider,
    ) -> rp::ColliderHandle {
        let h = self.world.insert_collider(collider, Some(body));
        self.collider_owner.insert(h, entity);
        h
    }

    /// ECS → rapier: remove stale bodies, create new ones, push game-side edits.
    pub(crate) fn sync_from_world(&mut self, world: &mut World) {
        // 1. Despawned entities / removed components.
        let stale: Vec<Entity> = self
            .entries
            .keys()
            .copied()
            .filter(|&e| !world.contains(e) || !world.satisfies::<Or<&RigidBody, &Collider>>(e))
            .collect();
        for e in stale {
            self.remove_entry(e);
        }

        // 2. Create / update.
        let mut missing_transform = Vec::new();
        {
            let mut q = world
                .query::<(
                    Entity,
                    Option<&RigidBody>,
                    Option<&Collider>,
                    Option<&Transform>,
                    Option<&Velocity>,
                    Option<&Restitution>,
                    Option<&Friction>,
                    Option<&Sensor>,
                    Option<&GravityScale>,
                )>()
                .with::<Or<&RigidBody, &Collider>>();
            let mut items: Vec<_> = q.iter().collect();
            // Deterministic insertion order regardless of archetype layout.
            items.sort_unstable_by_key(|i| i.0.id());
            for (e, rb, col, t, vel, rest, fric, sensor, gs) in items {
                let kind = rb.copied().unwrap_or(RigidBody::Fixed);
                let t = t.copied().unwrap_or_else(|| {
                    missing_transform.push(e);
                    Transform::IDENTITY
                });
                let restitution = rest.copied().unwrap_or_default().0;
                let friction = fric.copied().unwrap_or_default().0;
                let sensor = sensor.is_some();
                let gravity_scale = gs.copied().unwrap_or_default().0;
                let shape_key = col.map(|c| c.key(t.scale));

                let Some(entry) = self.entries.get_mut(&e) else {
                    // New body.
                    let builder = match kind {
                        RigidBody::Dynamic => rp::RigidBodyBuilder::dynamic(),
                        RigidBody::Fixed => rp::RigidBodyBuilder::fixed(),
                        RigidBody::KinematicPosition => rp::RigidBodyBuilder::kinematic_position_based(),
                    };
                    let v = vel.copied().unwrap_or_default();
                    let body = builder
                        .pose(pose(&t))
                        .linvel(v.linear)
                        .angvel(v.angular)
                        .gravity_scale(gravity_scale)
                        .user_data(entity_bits(e))
                        .build();
                    let body = self.world.insert_body(body);
                    let collider = col
                        .and_then(|c| Self::build_collider(e, c, t.scale, restitution, friction, sensor))
                        .map(|c| self.insert_collider(e, body, c));
                    self.entries.insert(
                        e,
                        BodyEntry {
                            body,
                            collider,
                            kind,
                            shape: shape_key,
                            pose: (t.translation, t.rotation),
                            vel: vel.copied(),
                        },
                    );
                    continue;
                };

                let Some(body) = self.world.bodies.get_mut(entry.body) else {
                    continue;
                };
                if entry.kind != kind {
                    entry.kind = kind;
                    let ty = match kind {
                        RigidBody::Dynamic => rp::RigidBodyType::Dynamic,
                        RigidBody::Fixed => rp::RigidBodyType::Fixed,
                        RigidBody::KinematicPosition => rp::RigidBodyType::KinematicPositionBased,
                    };
                    body.set_body_type(ty, true);
                }
                if body.gravity_scale() != gravity_scale {
                    body.set_gravity_scale(gravity_scale, true);
                }
                let p = (t.translation, t.rotation);
                match kind {
                    RigidBody::KinematicPosition => {
                        if p != entry.pose {
                            body.set_next_kinematic_position(pose(&t));
                            entry.pose = p;
                        }
                    }
                    RigidBody::Fixed | RigidBody::Dynamic => {
                        if p != entry.pose {
                            body.set_position(pose(&t), true);
                            entry.pose = p;
                        }
                    }
                }
                if kind == RigidBody::Dynamic
                    && let Some(v) = vel
                    && Some(*v) != entry.vel
                {
                    body.set_linvel(v.linear, true);
                    body.set_angvel(v.angular, true);
                    entry.vel = Some(*v);
                }

                // Collider edits.
                if shape_key != entry.shape {
                    entry.shape = shape_key;
                    if let Some(old) = entry.collider.take() {
                        self.world.remove_collider(old);
                        self.removed.push(old);
                    }
                    let body = entry.body;
                    if let Some(c) =
                        col.and_then(|c| Self::build_collider(e, c, t.scale, restitution, friction, sensor))
                    {
                        let h = self.world.insert_collider(c, Some(body));
                        self.collider_owner.insert(h, e);
                        entry.collider = Some(h);
                    }
                } else if let Some(c) = entry.collider.and_then(|h| self.world.colliders.get_mut(h)) {
                    if c.restitution() != restitution {
                        c.set_restitution(restitution);
                    }
                    if c.friction() != friction {
                        c.set_friction(friction);
                    }
                    if c.is_sensor() != sensor {
                        c.set_sensor(sensor);
                        if sensor {
                            c.set_active_collision_types(rp::ActiveCollisionTypes::all());
                        }
                    }
                }
            }
        }
        for e in missing_transform {
            let _ = world.insert_one(e, Transform::IDENTITY);
        }
    }

    /// Advance the simulation by `dt` seconds.
    pub(crate) fn step(&mut self, dt: f32) {
        if self.paused {
            return;
        }
        self.world.integration_parameters.dt = dt;
        self.world.step_with_events(&(), &self.collector);
        self.steps += 1;
    }

    /// rapier → ECS: write poses/velocities of moving bodies back into components.
    pub(crate) fn write_back(&mut self, world: &mut World) {
        for (&e, entry) in self.entries.iter_mut() {
            if entry.kind != RigidBody::Dynamic {
                continue;
            }
            let Some(body) = self.world.bodies.get(entry.body) else {
                continue;
            };
            let Ok((t, v)) = world.query_one_mut::<(&mut Transform, Option<&mut Velocity>)>(e) else {
                continue;
            };
            let p = body.position();
            t.translation = p.translation;
            t.rotation = p.rotation;
            entry.pose = (t.translation, t.rotation);
            if let Some(v) = v {
                v.linear = body.linvel();
                v.angular = body.angvel();
                entry.vel = Some(*v);
            }
        }
    }

    /// Convert buffered rapier events into entity events and forget removed colliders.
    pub(crate) fn drain_events(&mut self) -> Vec<CollisionEvent> {
        let raw = std::mem::take(&mut *self.collector.0.lock().unwrap_or_else(|p| p.into_inner()));
        let out = raw
            .into_iter()
            .filter_map(|ev| {
                let a = self.collider_entity(ev.collider1())?;
                let b = self.collider_entity(ev.collider2())?;
                Some(match ev {
                    rp::CollisionEvent::Started(_, _, flags) => CollisionEvent::Started {
                        a,
                        b,
                        sensor: flags.contains(rp::CollisionEventFlags::SENSOR),
                    },
                    rp::CollisionEvent::Stopped(_, _, flags) => CollisionEvent::Stopped {
                        a,
                        b,
                        sensor: flags.contains(rp::CollisionEventFlags::SENSOR),
                        removed: flags.contains(rp::CollisionEventFlags::REMOVED),
                    },
                })
            })
            .collect();
        if !self.paused {
            for h in self.removed.drain(..) {
                self.collider_owner.remove(&h);
            }
        }
        out
    }
}
