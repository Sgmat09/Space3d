//! Lag compensation ("favour the shooter", Source-style).
//!
//! Hitboxes of `LagCompensated` entities are recorded every tick (PostUpdate) into a ring of
//! `max_rewind_ticks`. A hit-scan from client C at server tick T rewinds to
//! `T − view_delay(C)` (clamped to `max_rewind`, default 200 ms) and interpolates between the
//! two bracketing ticks exactly as the client rendered them. The view delay comes from
//! `InputWindow::view_delay_q8` of C's latest input.

use super::NetServer;
use crate::{FastMap, NetClientId, NetId, Tick};
use space3d_core::glam::{Quat, Vec3};
use space3d_core::{Entity, SystemCtx, TickRate, Transform};
use std::collections::VecDeque;
use std::time::Duration;

/// Hit shape in the entity's local space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HitShape {
    /// Capsule along local Y.
    Capsule {
        /// Radius.
        radius: f32,
        /// Half height of the cylinder part.
        half_height: f32,
    },
    /// Axis-aligned box.
    Aabb {
        /// Half extents.
        half: Vec3,
    },
}

/// Marks an entity whose hitbox history is recorded.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LagCompensated {
    /// Hitbox.
    pub shape: HitShape,
}

/// Result of a rewound raycast.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LagHit {
    /// Entity hit.
    pub entity: Entity,
    /// Its wire id.
    pub net_id: NetId,
    /// Distance along the ray.
    pub distance: f32,
    /// Hit point (rewound space).
    pub point: Vec3,
}

/// Two bracketing recorded ticks (`b` is `None` when clamped to one end) and the fraction.
type Bracket<'a> = (&'a [Sample], Option<&'a [Sample]>, f32);

#[derive(Clone, Copy, Debug)]
struct Sample {
    entity: Entity,
    net_id: NetId,
    pos: Vec3,
    rot: Quat,
    shape: HitShape,
}

/// Hitbox history (resource; inserted by `ServerReplicationPlugin`). Recording is free when
/// no entity has [`LagCompensated`].
#[derive(Debug, Default)]
pub struct LagCompensation {
    max_rewind_ticks: u32,
    max_rewind: Duration,
    history: VecDeque<(Tick, Vec<Sample>)>,
    view_delays: FastMap<NetClientId, f32>,
}

impl LagCompensation {
    /// History covering `max_rewind` (default 200 ms).
    pub fn new(max_rewind: Duration) -> Self {
        Self {
            max_rewind,
            ..Default::default()
        }
    }

    /// Longest rewind in ticks.
    pub fn max_rewind_ticks(&self) -> u32 {
        self.max_rewind_ticks
    }

    /// Set a client's view delay in ticks (normally updated from its inputs every tick).
    pub fn set_view_delay(&mut self, client: NetClientId, ticks: f32) {
        self.view_delays.insert(client, ticks.max(0.0));
    }

    /// Record the hitboxes of `tick` (called by the plugin in PostUpdate).
    pub fn record(&mut self, tick: Tick, samples: impl IntoIterator<Item = (Entity, NetId, Transform, HitShape)>) {
        let v: Vec<Sample> = samples
            .into_iter()
            .map(|(entity, net_id, t, shape)| Sample {
                entity,
                net_id,
                pos: t.translation,
                rot: t.rotation,
                shape,
            })
            .collect();
        if self.history.back().is_some_and(|(t, _)| *t >= tick) {
            self.history.pop_back();
        }
        self.history.push_back((tick, v));
        while self.history.len() > self.max_rewind_ticks as usize + 2 {
            self.history.pop_front();
        }
    }

    /// Raycast against hitboxes as client `shooter` saw them at `tick` (excluding `exclude`).
    pub fn raycast(
        &self,
        shooter: NetClientId,
        tick: Tick,
        origin: Vec3,
        dir: Vec3,
        max_distance: f32,
        exclude: Option<Entity>,
    ) -> Option<LagHit> {
        let delay = self
            .view_delays
            .get(&shooter)
            .copied()
            .unwrap_or(0.0)
            .min(self.max_rewind_ticks as f32);
        self.raycast_at(f64::from(tick.0) - f64::from(delay), origin, dir, max_distance, exclude)
    }

    /// Raycast against the hitboxes interpolated at fractional tick `at`.
    pub fn raycast_at(
        &self,
        at: f64,
        origin: Vec3,
        dir: Vec3,
        max_distance: f32,
        exclude: Option<Entity>,
    ) -> Option<LagHit> {
        let dir = dir.try_normalize()?;
        let (a, b, frac) = self.bracket(at)?;
        let mut best: Option<LagHit> = None;
        for s in a {
            if Some(s.entity) == exclude {
                continue;
            }
            let (pos, rot) = match b.and_then(|b| b.iter().find(|o| o.entity == s.entity)) {
                Some(o) => (s.pos.lerp(o.pos, frac), s.rot.slerp(o.rot, frac)),
                None => (s.pos, s.rot),
            };
            if let Some(t) = intersect(s.shape, pos, rot, origin, dir)
                && t <= max_distance
                && best.is_none_or(|h| t < h.distance)
            {
                best = Some(LagHit {
                    entity: s.entity,
                    net_id: s.net_id,
                    distance: t,
                    point: origin + dir * t,
                });
            }
        }
        best
    }

    fn bracket(&self, at: f64) -> Option<Bracket<'_>> {
        let (first_t, first) = self.history.front()?;
        let (last_t, last) = self.history.back()?;
        if at <= f64::from(first_t.0) {
            return Some((first, None, 0.0));
        }
        if at >= f64::from(last_t.0) {
            return Some((last, None, 0.0));
        }
        let idx = self.history.partition_point(|(t, _)| f64::from(t.0) <= at);
        let (ta, a) = &self.history[idx - 1];
        let (tb, b) = &self.history[idx];
        let frac = ((at - f64::from(ta.0)) / f64::from(tb.0 - ta.0).max(1.0)) as f32;
        Some((a, Some(b), frac))
    }
}

/// Ray (normalized `rd`) vs shape at `pos`/`rot`; distance to the first hit (≥ 0).
fn intersect(shape: HitShape, pos: Vec3, rot: Quat, ro: Vec3, rd: Vec3) -> Option<f32> {
    match shape {
        HitShape::Capsule { radius, half_height } => {
            let axis = rot * Vec3::Y * half_height;
            capsule(ro, rd, pos - axis, pos + axis, radius)
        }
        HitShape::Aabb { half } => {
            let inv = rot.inverse();
            let o = inv * (ro - pos);
            let d = inv * rd;
            let mut tmin = 0.0f32;
            let mut tmax = f32::INFINITY;
            for i in 0..3 {
                if d[i].abs() < 1e-8 {
                    if o[i].abs() > half[i] {
                        return None;
                    }
                } else {
                    let t1 = (-half[i] - o[i]) / d[i];
                    let t2 = (half[i] - o[i]) / d[i];
                    tmin = tmin.max(t1.min(t2));
                    tmax = tmax.min(t1.max(t2));
                    if tmin > tmax {
                        return None;
                    }
                }
            }
            Some(tmin)
        }
    }
}

fn capsule(ro: Vec3, rd: Vec3, pa: Vec3, pb: Vec3, r: f32) -> Option<f32> {
    let sphere = |c: Vec3| -> Option<f32> {
        let oc = ro - c;
        let b = oc.dot(rd);
        let cc = oc.dot(oc) - r * r;
        if cc <= 0.0 {
            return Some(0.0);
        }
        let h = b * b - cc;
        (h >= 0.0 && -b - h.sqrt() >= 0.0).then(|| -b - h.sqrt())
    };
    let ba = pb - pa;
    let oa = ro - pa;
    let baba = ba.dot(ba);
    let bard = ba.dot(rd);
    let baoa = ba.dot(oa);
    let rdoa = rd.dot(oa);
    let oaoa = oa.dot(oa);
    let a = baba - bard * bard;
    let mut best: Option<f32> = None;
    if a > 1e-8 {
        let b = baba * rdoa - baoa * bard;
        let c = baba * oaoa - baoa * baoa - r * r * baba;
        let h = b * b - a * c;
        if h >= 0.0 {
            let t = (-b - h.sqrt()) / a;
            let y = baoa + t * bard;
            if t >= 0.0 && y > 0.0 && y < baba {
                best = Some(t);
            }
        }
    }
    for c in [pa, pb] {
        if let Some(t) = sphere(c) {
            best = Some(best.map_or(t, |b: f32| b.min(t)));
        }
    }
    best
}

/// PostUpdate system: record hitboxes and view delays for the current room tick.
pub(crate) fn record_system(ctx: &mut SystemCtx) {
    let hz = ctx.resources.get::<TickRate>().map_or(60.0, |r| r.hz);
    let Some(server) = ctx.resources.get::<NetServer>() else {
        return;
    };
    let tick = server.tick();
    let delays: Vec<(NetClientId, f32)> = server
        .slots
        .iter()
        .map(|s| (s.client, s.view_delay_q8 as f32 / 256.0))
        .collect();
    let samples: Vec<(Entity, NetId, Transform, HitShape)> = ctx
        .world
        .query::<(Entity, &LagCompensated, &Transform)>()
        .iter()
        .filter_map(|(e, lc, t)| Some((e, server.net_id(e)?, *t, lc.shape)))
        .collect();
    let lc = ctx.resources.init::<LagCompensation>();
    if lc.max_rewind.is_zero() {
        lc.max_rewind = Duration::from_millis(200);
    }
    lc.max_rewind_ticks = (lc.max_rewind.as_secs_f64() * hz).ceil() as u32;
    for (c, d) in delays {
        lc.view_delays.insert(c, d);
    }
    if samples.is_empty() && lc.history.is_empty() {
        return;
    }
    lc.record(tick, samples);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world_entity(i: u32) -> Entity {
        let mut w = space3d_core::World::new();
        let mut e = w.spawn(());
        for _ in 0..i {
            e = w.spawn(());
        }
        e
    }

    #[test]
    fn rewinds_to_what_the_shooter_saw() {
        let target = world_entity(0);
        let mut lc = LagCompensation::new(Duration::from_millis(200));
        lc.max_rewind_ticks = 12;
        let shape = HitShape::Capsule {
            radius: 0.5,
            half_height: 0.5,
        };
        // The target moves +1 m/tick along X.
        for t in 0..10u32 {
            lc.record(
                Tick(t),
                [(target, NetId(1), Transform::from_xyz(t as f32, 0.0, 0.0), shape)],
            );
        }
        let shooter = NetClientId(0);
        // Shooter saw the world 4.5 ticks ago: the target was at x = 9 - 4.5 = 4.5.
        lc.set_view_delay(shooter, 4.5);
        let hit = lc
            .raycast(shooter, Tick(9), Vec3::new(4.5, 0.0, -10.0), Vec3::Z, 100.0, None)
            .expect("hit");
        assert_eq!(hit.entity, target);
        assert!((hit.distance - 9.5).abs() < 1e-3, "{}", hit.distance);
        // Without rewind the shot misses.
        lc.set_view_delay(shooter, 0.0);
        assert!(
            lc.raycast(shooter, Tick(9), Vec3::new(4.5, 0.0, -10.0), Vec3::Z, 100.0, None)
                .is_none()
        );
        // Excluding the target and range limits.
        lc.set_view_delay(shooter, 4.5);
        assert!(
            lc.raycast(
                shooter,
                Tick(9),
                Vec3::new(4.5, 0.0, -10.0),
                Vec3::Z,
                100.0,
                Some(target)
            )
            .is_none()
        );
        assert!(
            lc.raycast(shooter, Tick(9), Vec3::new(4.5, 0.0, -10.0), Vec3::Z, 5.0, None)
                .is_none()
        );
    }

    #[test]
    fn aabb_rotated() {
        let t = intersect(
            HitShape::Aabb {
                half: Vec3::new(1.0, 1.0, 0.1),
            },
            Vec3::new(0.0, 0.0, 5.0),
            Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
            Vec3::ZERO,
            Vec3::Z,
        )
        .unwrap();
        assert!((t - 4.0).abs() < 1e-4, "{t}");
        assert!(
            intersect(
                HitShape::Aabb { half: Vec3::ONE },
                Vec3::new(5.0, 0.0, 0.0),
                Quat::IDENTITY,
                Vec3::ZERO,
                Vec3::Z
            )
            .is_none()
        );
    }
}
