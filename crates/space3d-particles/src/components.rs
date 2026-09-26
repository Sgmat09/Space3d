//! Components, events and spawning: [`ParticleEmitter`], [`ParticleBeam`],
//! [`ParticleAttractor`], [`ParticleEffectFinished`], [`EffectSpawn`] and [`SpawnEffect`].

use crate::effect::Falloff;
use crate::registry::EffectHandle;
use space3d_core::glam::{Mat4, Quat, Vec3, Vec4};
use space3d_core::{App, Entity, Parent, SystemCtx, Transform, World};

/// Plays a [`crate::ParticleEffect`] at this entity's `Transform` (+ optional `Parent`).
#[derive(Clone, Debug, PartialEq)]
pub struct ParticleEmitter {
    /// The effect.
    pub effect: EffectHandle,
    /// `false`: stop emitting; live particles and ribbons play out.
    pub playing: bool,
    /// × continuous rates (throttle), clamped to each layer's `max_rate_scale`.
    pub rate_scale: f32,
    /// Uniform effect scale (sizes, shape extents, speeds^exponent).
    pub scale: f32,
    /// Linear RGBA × `tinted` layers (rgb may exceed 1).
    pub tint: Vec4,
    /// Extra world velocity for `inherit_velocity` (the measured motion is added).
    pub velocity: Vec3,
    /// `None`: a hash of the entity bits, the first tick and `ParticleSettings::seed`.
    pub seed: Option<u32>,
    /// Seconds simulated instantly on first sight (ambient loops).
    pub prewarm: f32,
    /// When a non-looping effect has fully played.
    pub finish: FinishAction,
    /// When the component, entity or parent disappears.
    pub on_remove: OnRemove,
    /// Bump to restart from t = 0 (old particles drain).
    pub restart: u32,
    /// Seconds (0: off, the default): paces the time-rate (`Spawn::rate`) emission of
    /// world-space layers by the distance the emitter moves each tick, against its speed
    /// low-passed over about this time. Particles emitted in a tick are spread along that
    /// tick's motion, so an emitter moving in **steps** (a ship easing toward 20 Hz server
    /// updates: a long tick, then short or still ones) lays a trail that is sparse where it
    /// jumped and beaded where it paused; paced, each tick emits in proportion to how far it
    /// moved and the trail keeps an even density. The average rate is unchanged. `0.1`–`0.2`
    /// covers 20 Hz steps; an emitter that stops dead thins its time-rate emission for about
    /// twice this time. `rate_per_distance` is spatial already and unaffected.
    pub velocity_smoothing: f32,
}

impl ParticleEmitter {
    /// Playing, scale 1, tint white, `Keep`, `Drain`.
    pub fn new(effect: EffectHandle) -> Self {
        Self {
            effect,
            playing: true,
            rate_scale: 1.0,
            scale: 1.0,
            tint: Vec4::ONE,
            velocity: Vec3::ZERO,
            seed: None,
            prewarm: 0.0,
            finish: FinishAction::Keep,
            on_remove: OnRemove::Drain,
            restart: 0,
            velocity_smoothing: 0.0,
        }
    }
    /// A one-shot: despawned once it has fully played.
    pub fn one_shot(effect: EffectHandle) -> Self {
        Self {
            finish: FinishAction::Despawn,
            ..Self::new(effect)
        }
    }
    /// Set [`Self::tint`].
    pub fn with_tint(mut self, tint: Vec4) -> Self {
        self.tint = tint;
        self
    }
    /// Set [`Self::scale`].
    pub fn with_scale(mut self, scale: f32) -> Self {
        self.scale = scale;
        self
    }
    /// Set [`Self::rate_scale`].
    pub fn with_rate_scale(mut self, s: f32) -> Self {
        self.rate_scale = s;
        self
    }
    /// Set [`Self::seed`].
    pub fn with_seed(mut self, seed: u32) -> Self {
        self.seed = Some(seed);
        self
    }
    /// Set [`Self::prewarm`].
    pub fn with_prewarm(mut self, secs: f32) -> Self {
        self.prewarm = secs;
        self
    }
    /// Set [`Self::velocity_smoothing`].
    pub fn with_velocity_smoothing(mut self, secs: f32) -> Self {
        self.velocity_smoothing = secs;
        self
    }
    /// Stop emitting (`playing = false`).
    pub fn stop(&mut self) {
        self.playing = false;
    }
    /// Restart from t = 0 (`restart += 1`, `playing = true`).
    pub fn restart(&mut self) {
        self.restart = self.restart.wrapping_add(1);
        self.playing = true;
    }
}

/// What happens when a non-looping effect has fully played.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FinishAction {
    /// Nothing (the entity stays).
    #[default]
    Keep,
    /// Despawn the entity.
    Despawn,
    /// Remove the `ParticleEmitter` component.
    RemoveEmitter,
}

/// What happens to live particles when the emitter disappears.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum OnRemove {
    /// They play out.
    #[default]
    Drain,
    /// They vanish.
    Clear,
}

/// End point of the effect's beam layers (lasers). Without it, beams extend
/// `BeamLayer::length` along the emitter's forward (−Z).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParticleBeam {
    /// Where the beam ends.
    pub end: BeamEnd,
    /// × every beam layer's width.
    pub width_scale: f32,
}

/// Where a beam ends.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum BeamEnd {
    /// A world position.
    World(Vec3),
    /// A position local to the emitter.
    Local(Vec3),
    /// Another entity's position.
    Entity(Entity),
}

/// World-space force field affecting layers whose `world_forces & mask != 0` (at most 8 per
/// tick, the first 8 in query order).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParticleAttractor {
    /// Strength (negative repels).
    pub strength: f32,
    /// Radius of influence.
    pub radius: f32,
    /// Falloff.
    pub falloff: Falloff,
    /// Layer mask.
    pub mask: u32,
}

/// Sent when an effect instance has emitted everything and all its particles, ribbon points and
/// light flash are gone (before its `FinishAction` runs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParticleEffectFinished {
    /// The emitter entity.
    pub entity: Entity,
    /// Its effect.
    pub effect: EffectHandle,
}

/// One-call spawning of (usually one-shot) effects: see [`SpawnEffect`].
///
/// The transform is the emitter entity's `Transform`: a world transform, or, with
/// [`follow`](Self::follow), one **local to the followed entity** (it is inserted as that
/// entity's child). So `EffectSpawn::at(Vec3::ZERO).follow(ship)` plays at the ship and
/// `EffectSpawn::at_local(nozzle).follow(ship)` at an offset from it, while
/// `EffectSpawn::at(ship_pos).follow(ship)` would play at twice the ship's position. For a
/// world point on a moving entity (a hit on a hull), build the spawn in world space and
/// attach it with [`follow_world`](Self::follow_world), which converts it into the entity's
/// local space from the entity's current pose.
#[derive(Clone, Debug, PartialEq)]
pub struct EffectSpawn {
    /// World transform, or local to `follow`.
    pub transform: Transform,
    /// Inserted as `Parent`: the effect moves with that entity.
    pub follow: Option<Entity>,
    /// Linear RGBA tint.
    pub tint: Vec4,
    /// Uniform scale.
    pub scale: f32,
    /// Extra velocity.
    pub velocity: Vec3,
    /// Seed (`None`: derived from the entity).
    pub seed: Option<u32>,
    /// Seconds simulated instantly.
    pub prewarm: f32,
    /// End of the effect's beams.
    pub beam_end: Option<BeamEnd>,
    /// Default `Despawn`.
    pub finish: FinishAction,
}

impl EffectSpawn {
    /// At `position`: a world position, or, with [`follow`](Self::follow), an offset in the
    /// followed entity's local space (`at(Vec3::ZERO).follow(ship)` plays at the ship; see
    /// [`follow_world`](Self::follow_world) for a world point).
    pub fn at(position: Vec3) -> Self {
        Self::from_transform(Transform::from_xyz(position.x, position.y, position.z))
    }
    /// At `offset` in the local space of the entity it will [`follow`](Self::follow) (the same
    /// as [`at`](Self::at); the name says which space the position is in).
    pub fn at_local(offset: Vec3) -> Self {
        Self::at(offset)
    }
    /// At a transform (world, or local to the followed entity).
    pub fn from_transform(t: Transform) -> Self {
        Self {
            transform: t,
            follow: None,
            tint: Vec4::ONE,
            scale: 1.0,
            velocity: Vec3::ZERO,
            seed: None,
            prewarm: 0.0,
            beam_end: None,
            finish: FinishAction::Despawn,
        }
    }
    /// Local −Z along `dir` (a hit normal, a bolt direction): in world space, or with
    /// `follow`, in the followed entity's space ([`follow_world`](Self::follow_world) converts
    /// a world facing too).
    pub fn facing(mut self, dir: Vec3) -> Self {
        if let Some(d) = dir.try_normalize() {
            self.transform.rotation = Quat::from_rotation_arc(Vec3::NEG_Z, d);
        }
        self
    }
    /// Move with `entity` (inserted as `Parent`): the transform is then local to `entity`.
    pub fn follow(mut self, entity: Entity) -> Self {
        self.follow = Some(entity);
        self
    }
    /// Move with `entity`, keeping this spawn's **world** transform: it is converted into
    /// `entity`'s local space from the entity's current pose in `world` (its `Transform` and
    /// `Parent` chain, as the particles' tick reads it), then follows it like
    /// [`follow`](Self::follow). A shield hit on a moving ship:
    /// `EffectSpawn::at(hit).facing(-dir).follow_world(ship, &ctx.world)`. Without a
    /// `Transform` on `entity` the transform is kept as it is.
    pub fn follow_world(mut self, entity: Entity, world: &World) -> Self {
        if let Some(parent) = world_matrix(world, entity) {
            let det = parent.determinant();
            if det.is_finite() && det.abs() > 1e-12 {
                let local = parent.inverse() * self.transform.matrix();
                let (scale, rotation, translation) = local.to_scale_rotation_translation();
                self.transform = Transform {
                    translation,
                    rotation: rotation.normalize(),
                    scale,
                };
            }
        }
        self.follow = Some(entity);
        self
    }
    /// Set the tint.
    pub fn tint(mut self, tint: Vec4) -> Self {
        self.tint = tint;
        self
    }
    /// Set the scale.
    pub fn scale(mut self, scale: f32) -> Self {
        self.scale = scale;
        self
    }
    /// Set the extra velocity.
    pub fn velocity(mut self, v: Vec3) -> Self {
        self.velocity = v;
        self
    }
    /// Set the seed.
    pub fn seed(mut self, seed: u32) -> Self {
        self.seed = Some(seed);
        self
    }
    /// Set the prewarm time.
    pub fn prewarm(mut self, secs: f32) -> Self {
        self.prewarm = secs;
        self
    }
    /// End the effect's beams at `end`.
    pub fn beam_to(mut self, end: BeamEnd) -> Self {
        self.beam_end = Some(end);
        self
    }
    /// `finish = Keep` (the caller despawns).
    pub fn keep(mut self) -> Self {
        self.finish = FinishAction::Keep;
        self
    }
}

impl From<Vec3> for EffectSpawn {
    fn from(p: Vec3) -> Self {
        Self::at(p)
    }
}

impl From<Transform> for EffectSpawn {
    fn from(t: Transform) -> Self {
        Self::from_transform(t)
    }
}

/// One-call effect spawning on a `World`, an `App` or a `SystemCtx`.
pub trait SpawnEffect {
    /// Spawn an entity playing `effect`; returns it (move, stop or despawn it freely).
    fn spawn_effect(&mut self, effect: EffectHandle, spawn: impl Into<EffectSpawn>) -> Entity;
    /// `playing = false`; false if `entity` has no emitter.
    fn stop_effect(&mut self, entity: Entity) -> bool;
}

/// World matrix of `e` through its `Parent` chain of `Transform`s (`None` without a
/// `Transform`).
fn world_matrix(world: &World, e: Entity) -> Option<Mat4> {
    let mut m = world.get::<&Transform>(e).ok()?.matrix();
    let mut next = world.get::<&Parent>(e).ok().map(|p| p.0);
    for _ in 0..32 {
        let Some(p) = next else {
            break;
        };
        let Ok(t) = world.get::<&Transform>(p) else {
            break;
        };
        m = t.matrix() * m;
        next = world.get::<&Parent>(p).ok().map(|p| p.0);
    }
    Some(m)
}

fn spawn_in(world: &mut World, effect: EffectHandle, s: EffectSpawn) -> Entity {
    let emitter = ParticleEmitter {
        tint: s.tint,
        scale: s.scale,
        velocity: s.velocity,
        seed: s.seed,
        prewarm: s.prewarm,
        finish: s.finish,
        ..ParticleEmitter::new(effect)
    };
    let e = world.spawn((s.transform, emitter));
    if let Some(parent) = s.follow {
        let _ = world.insert_one(e, Parent(parent));
    }
    if let Some(end) = s.beam_end {
        let _ = world.insert_one(e, ParticleBeam { end, width_scale: 1.0 });
    }
    e
}

fn stop_in(world: &mut World, entity: Entity) -> bool {
    match world.get::<&mut ParticleEmitter>(entity) {
        Ok(mut e) => {
            e.stop();
            true
        }
        Err(_) => false,
    }
}

impl SpawnEffect for World {
    fn spawn_effect(&mut self, effect: EffectHandle, spawn: impl Into<EffectSpawn>) -> Entity {
        spawn_in(self, effect, spawn.into())
    }
    fn stop_effect(&mut self, entity: Entity) -> bool {
        stop_in(self, entity)
    }
}

impl SpawnEffect for App {
    fn spawn_effect(&mut self, effect: EffectHandle, spawn: impl Into<EffectSpawn>) -> Entity {
        spawn_in(&mut self.world, effect, spawn.into())
    }
    fn stop_effect(&mut self, entity: Entity) -> bool {
        stop_in(&mut self.world, entity)
    }
}

impl SpawnEffect for SystemCtx<'_> {
    fn spawn_effect(&mut self, effect: EffectHandle, spawn: impl Into<EffectSpawn>) -> Entity {
        spawn_in(self.world, effect, spawn.into())
    }
    fn stop_effect(&mut self, entity: Entity) -> bool {
        stop_in(self.world, entity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin;

    #[test]
    fn spawn_and_stop() {
        let mut world = World::new();
        let ship = world.spawn(());
        let e = world.spawn_effect(
            builtin::HIT_SPARK,
            EffectSpawn::at(Vec3::X)
                .facing(Vec3::Y)
                .follow(ship)
                .tint(Vec4::new(1.0, 0.5, 0.0, 1.0))
                .beam_to(BeamEnd::World(Vec3::ZERO)),
        );
        let em = (*world.get::<&ParticleEmitter>(e).unwrap()).clone();
        assert_eq!(
            (em.effect, em.finish, em.tint.y),
            (builtin::HIT_SPARK, FinishAction::Despawn, 0.5)
        );
        assert_eq!(world.get::<&Parent>(e).unwrap().0, ship);
        let t = *world.get::<&Transform>(e).unwrap();
        assert!((t.rotation * Vec3::NEG_Z - Vec3::Y).length() < 1e-5);
        assert!(world.get::<&ParticleBeam>(e).is_ok());
        assert!(world.stop_effect(e));
        assert!(!world.get::<&ParticleEmitter>(e).unwrap().playing);
        assert!(!world.stop_effect(ship));
        let mut m = ParticleEmitter::one_shot(builtin::EXPLOSION).with_scale(2.0);
        m.restart();
        assert_eq!((m.restart, m.playing, m.scale), (1, true, 2.0));
        let kept: EffectSpawn = Transform::IDENTITY.into();
        assert_eq!(kept.keep().finish, FinishAction::Keep);
    }

    /// game#1: `at(world).follow(parent)` is local to the parent (the design's "world, or
    /// local to `follow`"): `at_local` names it, and `follow_world` converts a world transform
    /// into the parent's space, so a world hit point plays there on a moved, turned, scaled and
    /// nested parent.
    #[test]
    fn follow_world_keeps_the_world_transform() {
        let mut world = World::new();
        let root = world.spawn((Transform::from_xyz(10.0, 0.0, -5.0).with_rotation(Quat::from_rotation_y(0.7)),));
        let ship = world.spawn((
            Transform::from_xyz(100.0, 0.0, 0.0)
                .with_rotation(Quat::from_rotation_y(1.1))
                .with_scale(Vec3::splat(2.0)),
            Parent(root),
        ));
        let hit = Vec3::new(97.0, 3.0, 40.0);
        let dir = Vec3::new(0.3, 0.0, -1.0).normalize();
        let e = world.spawn_effect(
            builtin::SHIELD_RIPPLE,
            EffectSpawn::at(hit).facing(dir).follow_world(ship, &world),
        );
        assert_eq!(world.get::<&Parent>(e).unwrap().0, ship);
        let m = world_matrix(&world, e).unwrap();
        let (_, rot, pos) = m.to_scale_rotation_translation();
        assert!((pos - hit).length() < 1e-3, "{pos} vs {hit}");
        assert!((rot * Vec3::NEG_Z - dir).length() < 1e-4, "{}", rot * Vec3::NEG_Z);
        // `at` / `at_local` with `follow`: an offset in the parent's space.
        let local = world.spawn_effect(builtin::HIT_SPARK, EffectSpawn::at_local(Vec3::ZERO).follow(ship));
        let p = world_matrix(&world, local).unwrap().w_axis.truncate();
        let s = world_matrix(&world, ship).unwrap().w_axis.truncate();
        assert!((p - s).length() < 1e-4);
        assert_eq!(EffectSpawn::at_local(Vec3::X), EffectSpawn::at(Vec3::X));
        // No transform on the entity: kept as it is.
        let bare = world.spawn(());
        let kept = EffectSpawn::at(hit).follow_world(bare, &world);
        assert_eq!((kept.transform.translation, kept.follow), (hit, Some(bare)));
    }
}
