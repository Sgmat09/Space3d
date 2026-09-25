//! A sprite layer compiled for the simulation and the instance pack: plain `Copy` data per layer
//! instance (no heap), and the capacity / life window bounds of design-particles §5.1.

use crate::curve::FloatRange;
use crate::effect::{
    EmitDirection, EmitShape, Facing, Flicker, Force, MAX_FORCES_PER_LAYER, ParticleBlend, ParticleLayer,
    SimulationSpace,
};
use space3d_core::glam::{Vec3, Vec4};

/// Largest ring per layer instance.
pub(crate) const MAX_LAYER_CAPACITY: u32 = 65_536;
/// Reference emitter speed for `rate_per_distance` capacities (world units per second).
const V_REF: f32 = 100.0;

/// Simulation and per-particle attribute parameters of one sprite layer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct LayerSim {
    pub shape: EmitShape,
    pub direction: EmitDirection,
    pub spread: f32,
    pub offset: Vec3,
    pub flatten: f32,
    pub flatten_axis: Vec3,
    pub local: bool,
    pub lifetime: (f32, f32),
    pub delay: (f32, f32),
    pub speed: (f32, f32),
    pub speed_exponent: f32,
    pub inherit: f32,
    pub drag: f32,
    pub gravity: Vec3,
    pub forces: [Option<Force>; MAX_FORCES_PER_LAYER],
    pub world_forces: u32,
    pub size: (f32, f32),
    pub rotation: (f32, f32),
    pub angular: (f32, f32),
    pub mirror: bool,
    pub color_a: Vec4,
    pub color_b: Vec4,
    pub coverage: (f32, f32),
    pub flicker: Option<Flicker>,
    pub facing: Facing,
    pub blend: ParticleBlend,
    pub tinted: bool,
    /// Longest life after emission, delay included (seconds).
    pub window: f32,
    /// Bound of the particles' distance from the emitter ([`Self::reach`]).
    pub reach: Reach,
    /// Continuous emission (the render-time bridge, `Runtime::bridge`).
    pub spawn: super::spawn::Continuous,
}

fn pair(r: FloatRange) -> (f32, f32) {
    (r.min, r.max)
}

impl LayerSim {
    pub fn new(l: &ParticleLayer) -> Self {
        let mut forces = [None; MAX_FORCES_PER_LAYER];
        for (slot, f) in forces.iter_mut().zip(&l.forces) {
            *slot = Some(*f);
        }
        let facing = match l.render.facing {
            Facing::Plane { normal } => Facing::Plane {
                normal: normal.normalize_or(Vec3::Y),
            },
            f => f,
        };
        Self {
            shape: l.shape,
            direction: l.direction,
            spread: l.spread.max(0.0),
            offset: l.offset,
            flatten: l.flatten.clamp(0.0, 1.0),
            flatten_axis: l.flatten_axis.normalize_or(Vec3::Y),
            local: l.space == SimulationSpace::Local,
            lifetime: pair(l.lifetime),
            delay: (l.particle_delay.min.max(0.0), l.particle_delay.max.max(0.0)),
            speed: pair(l.speed),
            speed_exponent: l.speed_scale_exponent,
            inherit: l.inherit_velocity,
            drag: l.drag.max(0.0),
            gravity: l.gravity,
            forces,
            world_forces: l.world_forces,
            size: pair(l.size),
            rotation: pair(l.rotation),
            angular: pair(l.angular_velocity),
            mirror: l.random_mirror,
            color_a: Vec4::from_array(l.start_color.a),
            color_b: Vec4::from_array(l.start_color.b),
            coverage: pair(l.coverage),
            flicker: l.flicker,
            facing,
            blend: l.render.blend,
            tinted: l.tinted,
            window: window_secs(l),
            reach: Reach::new(l, window_secs(l)),
            spawn: super::spawn::Continuous::new(l),
        }
    }
}

/// The parts of [`LayerSim::reach`] that do not depend on the instance (computed once).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Reach {
    /// Spawn shape extent plus the offset (× effect scale).
    shape: f32,
    /// Largest initial speed (× `scale^speed_exponent`).
    speed: f32,
    /// Distance covered per unit of initial speed over the life window (with drag).
    time: f32,
    /// Distance covered under gravity and the layer's forces over the life window (infinite
    /// with world attractors: they can be anywhere).
    accel: f32,
}

impl Reach {
    fn new(l: &ParticleLayer, window: f32) -> Self {
        let shape = match l.shape {
            EmitShape::Point => 0.0,
            EmitShape::Sphere { radius, .. } => radius.abs(),
            EmitShape::Cone { radius, .. } => radius.abs(),
            EmitShape::Ring { radius, width, .. } => radius.abs() + 0.5 * width.abs(),
            EmitShape::Box { half_extents } => half_extents.abs().length(),
            EmitShape::Line { length } => 0.5 * length.abs(),
        } + l.offset.length();
        // One tick more than the life window: extrapolation and the sub-tick pre-aging.
        let t = window + 0.1;
        let drag = l.drag.max(0.0);
        let time = if drag > 1e-6 {
            (1.0 - (-drag * t).exp()) / drag
        } else {
            t
        };
        let mut accel = l.gravity.length();
        for f in &l.forces {
            accel += match *f {
                // `attract` caps the inverse square at 1 / 0.01.
                Force::Attractor { strength, falloff, .. } => {
                    strength.abs()
                        * if matches!(falloff, crate::effect::Falloff::InverseSquare) {
                            100.0
                        } else {
                            1.0
                        }
                }
                Force::Vortex { strength, pull, .. } => strength.abs() + pull.abs(),
                Force::Turbulence { strength, .. } => strength.abs() * 3f32.sqrt(),
            };
        }
        let accel = if l.world_forces != 0 {
            f32::INFINITY
        } else {
            0.5 * accel * t * t
        };
        Self {
            shape,
            speed: l.speed.min.abs().max(l.speed.max.abs()),
            time,
            accel,
        }
    }
}

impl LayerSim {
    /// Upper bound of how far from its emitter a particle of this layer gets (world units, the
    /// sprite not included), for an effect `scale` and an emitter moving at most at
    /// `emitter_speed` (world-space layers inherit part of it): the spawn shape's extent plus
    /// the displacement over the whole life window under the initial speed (with drag), gravity
    /// and the layer's forces. Infinite with world attractors (they can be anywhere).
    /// Design-particles §5.1 "extent R".
    pub fn reach(&self, scale: f32, emitter_speed: f32) -> f32 {
        let r = &self.reach;
        let scale = scale.abs();
        let mut speed = r.speed;
        if self.speed_exponent != 0.0 {
            speed *= scale.powf(self.speed_exponent);
        }
        if !self.local {
            speed += self.inherit.abs() * emitter_speed;
        }
        let out = r.shape * scale + speed * r.time + r.accel;
        if out.is_finite() { out } else { f32::INFINITY }
    }
}

/// Longest particle life after emission, delay included.
pub(crate) fn window_secs(l: &ParticleLayer) -> f32 {
    l.lifetime.max.max(l.lifetime.min).max(0.0) + l.particle_delay.max.max(0.0)
}

/// Ticks after its last emission by which every particle of a layer is dead:
/// `ceil(window / dt) + 1` (the particle emitted latest in a tick lives at most
/// `ceil(window / dt)` more ticks; one more absorbs the rounding of the ages' f32 sums).
pub(crate) fn window_ticks(window: f32, dt: f32) -> u32 {
    if dt <= 0.0 {
        return u32::MAX;
    }
    ((window.max(0.0) as f64 / dt as f64).ceil().min(u32::MAX as f64 / 2.0) as u32) + 1
}

/// Ring size of a layer instance (design-particles §5.1): continuous rate × max rate scale ×
/// window × 1.05, the bursts that can overlap within the window, the distance rate at 100 u/s,
/// clamped to 1..=65 536; `ParticleLayer::capacity` overrides it.
pub(crate) fn capacity(l: &ParticleLayer) -> u32 {
    if let Some(c) = l.capacity {
        return c.clamp(1, MAX_LAYER_CAPACITY);
    }
    let win = window_secs(l);
    let s = &l.spawn;
    let scale = s.max_rate_scale.max(1.0).max(0.0);
    let mut n = 0.0f64;
    if s.rate > 0.0 {
        n += (s.rate as f64 * scale as f64 * win as f64 * 1.05).ceil() + 1.0;
    }
    if s.rate_per_distance > 0.0 {
        n += (s.rate_per_distance as f64 * scale as f64 * V_REF as f64 * win as f64).ceil() + 1.0;
    }
    for b in &s.bursts {
        let overlapping = if b.interval > 0.0 {
            let within = (win / b.interval).floor() as u64 + 1;
            if b.cycles == 0 {
                within
            } else {
                within.min(b.cycles as u64)
            }
        } else {
            b.cycles.max(1) as u64
        };
        n += (b.count.max.max(b.count.min) as u64 * overlapping) as f64;
    }
    (n.min(MAX_LAYER_CAPACITY as f64) as u32).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effect::Burst;

    #[test]
    fn capacities_cover_rates_and_bursts() {
        let l = ParticleLayer::continuous(60.0, (0.5, 1.0));
        assert_eq!(capacity(&l), (60.0f64 * 1.0 * 1.05).ceil() as u32 + 1);
        let mut l = ParticleLayer::burst((10, 20), 0.5);
        assert_eq!(capacity(&l), 20);
        l.spawn.bursts = vec![Burst {
            count: 5.into(),
            cycles: 0,
            interval: 0.2,
            ..Burst::default()
        }];
        assert_eq!(capacity(&l), 5 * 3, "cycles every 0.2 s overlapping a 0.5 s life");
        l.capacity = Some(7);
        assert_eq!(capacity(&l), 7);
        l.capacity = Some(1 << 20);
        assert_eq!(capacity(&l), MAX_LAYER_CAPACITY);
        assert_eq!(capacity(&ParticleLayer::default()), 1, "never zero");
        assert_eq!(window_ticks(0.5, 1.0 / 60.0), 31);
        assert_eq!(window_ticks(0.51, 1.0 / 60.0), 32);
        assert_eq!(window_ticks(0.0, 1.0 / 60.0), 1);
        assert_eq!(window_ticks(1.0, 0.0), u32::MAX);
    }
}
