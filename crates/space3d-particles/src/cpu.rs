//! The CPU simulation backend (WebGL2, GLES 3.0, and `ParticleBackend::Cpu`): the pool of
//! [`Particle`]s, the per-tick `simulate` / `emit` kernels in Rust (the GPU backend runs the same
//! steps as compute kernels) and the pack into the render instance stream.
//!
//! Every particle is a pure function of the logged ticks: its slot is `base + index mod
//! capacity`, its random attributes are `rnd(seed, stream)` of a seed keyed by (layer instance
//! seed, spawn index), and it reads nothing from other particles. Random attributes that never
//! change after the spawn (size, color, rotation and spin, coverage, flicker phase) are derived
//! from the seed at pack time instead of being stored.

use crate::effect::{EmitDirection, EmitShape, Facing, Falloff, Force, ParticleBlend};
use crate::pack::{f16_bits, oct_decode, oct_encode};
use crate::rng::{GOLDEN, particle_seed, pcg, range, range_exact, rnd, stream};
use crate::runtime::{Entry, LayerSim, Runtime, SpawnRecord, WorldAttractor, XformRow};
use space3d_core::glam::{Quat, Vec3, Vec4};
use std::f32::consts::TAU;

/// A pool slot (48 bytes, the GPU backend's storage layout, `S3dpParticle` in `gpu.wgsl`). Zero
/// = dead.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct Particle {
    /// Position (simulation space), age in seconds (< 0: delayed, hidden).
    pub pos_age: [f32; 4],
    /// Velocity (simulation space), lifetime in seconds (0 = dead). A particle dies on the tick
    /// its age reaches its lifetime: `age >= life`, two exactly computed values, so the CPU and
    /// GPU backends agree on every death.
    pub vel_life: [f32; 4],
    /// Octahedral emitter X axis at spawn, seed, layer instance (entry), octahedral emitter Y
    /// axis at spawn (`Facing::EmitterPlane`).
    pub misc: [u32; 4],
}

impl Particle {
    #[cfg(test)]
    pub fn alive(&self) -> bool {
        self.vel_life[3] != 0.0
    }
    pub fn pos(&self) -> Vec3 {
        Vec3::new(self.pos_age[0], self.pos_age[1], self.pos_age[2])
    }
    pub fn vel(&self) -> Vec3 {
        Vec3::new(self.vel_life[0], self.vel_life[1], self.vel_life[2])
    }
}

/// A render instance (40 bytes, per-instance vertex attributes): written by the CPU pack (or
/// the GPU backend's scatter kernel), read by the sprite vertex shader only.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct ParticleInstance {
    /// `Float32x4`: world position, signed size (negative: mirrored).
    pub pos_size: [f32; 4],
    /// `Float16x4`: world velocity (the plane normal for plane facings), age / life.
    pub vel_age: [u16; 4],
    /// `Float16x4`: linear tint (HDR), alpha (coverage for premultiplied layers).
    pub tint: [u16; 4],
    /// `Uint32x2`: x = rotation (f32 bits, wrapped to ±π), y = random bits.
    pub rot_frame: [u32; 2],
}

/// Bytes per render instance.
pub(crate) const INSTANCE_SIZE: u64 = std::mem::size_of::<ParticleInstance>() as u64;

/// Sub-tick resolution of emission times: a spawn record's span is quantized to 1/16384 of a
/// tick (about 1 µs at 60 Hz), so both backends derive identical spawn times and ages with
/// integer arithmetic.
pub(crate) const SUBTICKS: u32 = 16_384;

/// `t` (a fraction of the tick, 0..=1) in sub-tick units.
pub(crate) fn subtick_q(t: f32) -> u32 {
    (t.clamp(0.0, 1.0) * SUBTICKS as f32).round() as u32
}

/// Sub-tick time of particle `k` of `count` spread over `t0..t1` (sub-tick units): the middle of
/// its share of the span, `t0 + (t1 − t0)(2k + 1) / 2count` in integers (`(t1 − t0) ≤ 2¹⁴` and
/// `2k + 1 < 2¹⁷`, so the product fits a `u32`).
pub(crate) fn subtick(t0: u32, t1: u32, k: u32, count: u32) -> u32 {
    if t1 > t0 {
        t0 + ((t1 - t0) * (2 * k + 1)) / (2 * count)
    } else {
        t0
    }
}

/// A uniformly distributed unit vector from two uniforms.
fn uniform_dir(u: f32, v: f32) -> Vec3 {
    let z = 1.0 - 2.0 * u;
    let r = (1.0 - z * z).max(0.0).sqrt();
    let a = TAU * v;
    Vec3::new(r * a.cos(), r * a.sin(), z)
}

/// A direction in the cone of half-angle `half` around the unit `axis`.
fn cone_dir(axis: Vec3, half: f32, u: f32, v: f32) -> Vec3 {
    let cos_t = 1.0 - u * (1.0 - half.min(std::f32::consts::PI).cos());
    let sin_t = (1.0 - cos_t * cos_t).max(0.0).sqrt();
    let a = TAU * v;
    let (bx, by) = axis.any_orthonormal_pair();
    (axis * cos_t + (bx * a.cos() + by * a.sin()) * sin_t).normalize_or(axis)
}

/// Emitter-local offset and "outward" direction of a spawn point of `shape` at `scale`.
fn sample_shape(shape: EmitShape, seed: u32, scale: f32) -> (Vec3, Vec3) {
    let (u, v, w) = (
        rnd(seed, stream::SHAPE_A),
        rnd(seed, stream::SHAPE_B),
        rnd(seed, stream::SHAPE_C),
    );
    match shape {
        EmitShape::Point => (Vec3::ZERO, uniform_dir(u, v)),
        EmitShape::Sphere { radius, inner } => {
            let d = uniform_dir(u, v);
            let i3 = inner.clamp(0.0, 1.0).powi(3);
            (d * radius * (i3 + (1.0 - i3) * w).cbrt() * scale, d)
        }
        EmitShape::Cone { angle, radius } => {
            let a = TAU * u;
            let r = radius * v.sqrt() * scale;
            let dir = cone_dir(Vec3::NEG_Z, angle, w, rnd(seed, stream::DIR_B));
            (Vec3::new(r * a.cos(), r * a.sin(), 0.0), dir)
        }
        EmitShape::Ring { radius, width, arc } => {
            let arc = if arc > 0.0 { arc } else { TAU };
            let a = arc * u;
            let r = (radius + (v - 0.5) * width) * scale;
            let out = Vec3::new(a.cos(), 0.0, a.sin());
            (out * r, out)
        }
        EmitShape::Box { half_extents } => {
            let p = Vec3::new(u * 2.0 - 1.0, v * 2.0 - 1.0, w * 2.0 - 1.0) * half_extents * scale;
            let out = p
                .try_normalize()
                .unwrap_or_else(|| uniform_dir(rnd(seed, stream::DIR_A), rnd(seed, stream::DIR_B)));
            (p, out)
        }
        EmitShape::Line { length } => (Vec3::new((u - 0.5) * length * scale, 0.0, 0.0), Vec3::NEG_Z),
    }
}

/// Position and velocity after `t` seconds under constant `gravity` and exponential `drag`
/// (closed form; the pre-aging of particles emitted within a tick or synthesized by prewarm).
fn pre_age(x: Vec3, v: Vec3, gravity: Vec3, drag: f32, t: f32) -> (Vec3, Vec3) {
    if drag > 1e-6 {
        let e = (-drag * t).exp();
        let k = (1.0 - e) / drag;
        (x + v * k + gravity * ((t - k) / drag), v * e + gravity * k)
    } else {
        (x + v * t + gravity * (0.5 * t * t), v + gravity * t)
    }
}

/// Value noise in 0..1 on the integer lattice (smoothstep-blended corners).
fn value_noise(p: Vec3, seed: u32) -> f32 {
    let i = p.floor();
    let f = p - i;
    let w = f * f * (Vec3::splat(3.0) - 2.0 * f);
    let (x, y, z) = (i.x as i32, i.y as i32, i.z as i32);
    let h = |dx: i32, dy: i32, dz: i32| {
        let k = pcg((x + dx) as u32 ^ pcg((y + dy) as u32 ^ pcg((z + dz) as u32 ^ seed)));
        (k >> 8) as f32 * (1.0 / 16_777_216.0)
    };
    let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
    let x0 = lerp(
        lerp(h(0, 0, 0), h(1, 0, 0), w.x),
        lerp(h(0, 1, 0), h(1, 1, 0), w.x),
        w.y,
    );
    let x1 = lerp(
        lerp(h(0, 0, 1), h(1, 0, 1), w.x),
        lerp(h(0, 1, 1), h(1, 1, 1), w.x),
        w.y,
    );
    lerp(x0, x1, w.z)
}

fn attract(center: Vec3, x: Vec3, strength: f32, radius: f32, falloff: Falloff) -> Vec3 {
    let d = center - x;
    let dist = d.length();
    if dist < 1e-5 || (radius > 0.0 && dist > radius) {
        return Vec3::ZERO;
    }
    let k = match falloff {
        Falloff::Constant => 1.0,
        Falloff::Linear if radius > 0.0 => 1.0 - dist / radius,
        Falloff::Linear => 1.0,
        Falloff::InverseSquare => 1.0 / (dist * dist).max(0.01),
    };
    d * (strength * k / dist)
}

/// Acceleration from the layer's forces and the world attractors on a particle at `x`.
fn forces(s: &LayerSim, x: Vec3, row: &XformRow, time: f32, world: &[WorldAttractor]) -> Vec3 {
    let mut a = Vec3::ZERO;
    for f in s.forces.iter().flatten() {
        a += match *f {
            Force::Attractor {
                position,
                strength,
                radius,
                falloff,
            } => {
                let c = if s.local {
                    position * row.scale
                } else {
                    row.pos + row.rot * (position * row.scale)
                };
                attract(c, x, strength, radius * row.scale, falloff)
            }
            Force::Vortex { axis, strength, pull } => {
                let (o, ax) = if s.local {
                    (Vec3::ZERO, axis.normalize_or(Vec3::Y))
                } else {
                    (row.pos, (row.rot * axis).normalize_or(Vec3::Y))
                };
                let r = x - o;
                let rp = r - ax * r.dot(ax);
                let d = rp.length();
                if d < 1e-5 {
                    Vec3::ZERO
                } else {
                    let out = rp / d;
                    ax.cross(out) * strength - out * pull
                }
            }
            Force::Turbulence {
                strength,
                frequency,
                speed,
            } => {
                let p = x * frequency + Vec3::splat(time * speed);
                Vec3::new(
                    value_noise(p, 0x1f12_3bb5),
                    value_noise(p, 0x9e37_79b9),
                    value_noise(p, 0x7feb_352d),
                ) * (2.0 * strength)
                    - Vec3::splat(strength)
            }
        };
    }
    if s.world_forces != 0 {
        for w in world.iter().filter(|w| w.mask & s.world_forces != 0) {
            let c = if s.local {
                row.rot.inverse() * (w.position - row.pos)
            } else {
                w.position
            };
            a += attract(c, x, w.strength, w.radius, w.falloff);
        }
    }
    a
}

/// The CPU simulation state (owned by the render node).
#[derive(Debug, Default)]
pub(crate) struct CpuSim {
    pub pool: Vec<Particle>,
    /// Emitter frames of the tick being replayed, by instance index.
    xf: Vec<XformRow>,
    /// Drag factor of every entry for the current substep.
    drag: Vec<f32>,
    /// Ticks replayed by the last `replay`.
    pub ticks: u32,
    /// Pool resets (catch-up windows exceeded) since the start.
    pub resets: u32,
    /// Emissions that overwrote a particle still alive (a ring smaller than the layer's peak).
    pub overwrites: u64,
    warned_catchup: bool,
    warned_overwrite: bool,
}

impl CpuSim {
    fn capacities(&self) -> [usize; 3] {
        [self.pool.capacity(), self.xf.capacity(), self.drag.capacity()]
    }

    /// Replay every logged tick (at most `max_catchup`) into the pool; returns the collections
    /// that grew.
    pub fn replay(&mut self, rt: &mut Runtime, max_catchup: u32) -> u32 {
        let before = self.capacities();
        self.ticks = 0;
        let log = &mut rt.log;
        if log.frames.is_empty() {
            return 0;
        }
        if log.reset {
            self.pool.fill(Particle::default());
            self.resets += 1;
        }
        let max = max_catchup.max(1) as usize;
        let skip = log.frames.len().saturating_sub(max);
        if skip > 0 {
            if !self.warned_catchup {
                self.warned_catchup = true;
                tracing::warn!(
                    ticks = log.frames.len(),
                    max_catchup,
                    "particles: more pending ticks than ParticleSettings::max_catchup_ticks; the simulation restarts from the last ones (approximate)"
                );
            }
            self.pool.fill(Particle::default());
            self.resets += 1;
        }
        let need = rt.pool.high_water() as usize;
        if self.pool.len() < need {
            self.pool.resize(need, Particle::default());
        }
        if self.drag.len() < rt.entries.len() {
            self.drag.resize(rt.entries.len(), 1.0);
        }
        for f in &log.frames[skip..] {
            for row in &log.rows[f.rows.0 as usize..f.rows.1 as usize] {
                let i = row.instance as usize;
                if self.xf.len() <= i {
                    self.xf.resize(i + 1, XformRow::IDENTITY);
                }
                self.xf[i] = *row;
            }
            let attractors = &log.attractors[f.attractors.0 as usize..f.attractors.1 as usize];
            let substeps = f.substeps.max(1);
            let sub = f.dt / substeps as f32;
            if sub > 0.0 {
                for (d, e) in self.drag.iter_mut().zip(&rt.entries) {
                    *d = if e.sim.drag > 0.0 {
                        (-e.sim.drag * sub).exp()
                    } else {
                        1.0
                    };
                }
                for s in 0..substeps {
                    let time = (f.time + (s as f32 * sub) as f64) as f32;
                    simulate(&mut self.pool, sub, time, &rt.entries, &self.drag, &self.xf, attractors);
                }
            }
            for &(start, len) in &log.clears[f.clears.0 as usize..f.clears.1 as usize] {
                let end = (start + len).min(self.pool.len() as u32);
                self.pool[start.min(end) as usize..end as usize].fill(Particle::default());
            }
            for r in &log.records[f.records.0 as usize..f.records.1 as usize] {
                self.overwrites += emit(&mut self.pool, r, f.dt, &rt.entries, &self.xf);
            }
            self.ticks += 1;
        }
        log.clear();
        if self.overwrites > 0 && !self.warned_overwrite {
            self.warned_overwrite = true;
            tracing::warn!(
                "particles: a layer emitted faster than its ring holds, the oldest particles were replaced early (set ParticleLayer::capacity or raise Spawn::max_rate_scale)"
            );
        }
        let after = self.capacities();
        before.iter().zip(&after).filter(|(a, b)| a != b).count() as u32
    }
}

/// One substep of every live particle: age, death, forces, `v = (v + a·dt)·drag`, `x += v·dt`.
fn simulate(
    pool: &mut [Particle],
    dt: f32,
    time: f32,
    entries: &[Entry],
    drag: &[f32],
    xf: &[XformRow],
    attractors: &[WorldAttractor],
) {
    for p in pool.iter_mut() {
        let life = p.vel_life[3];
        if life == 0.0 {
            continue;
        }
        let age0 = p.pos_age[3];
        let age = age0 + dt;
        p.pos_age[3] = age;
        if age0 < 0.0 {
            continue; // delayed: only ages
        }
        if age >= life {
            p.vel_life[3] = 0.0;
            continue;
        }
        let e = p.misc[2] as usize;
        let Some(entry) = entries.get(e) else {
            p.vel_life[3] = 0.0;
            continue;
        };
        let s = &entry.sim;
        let mut x = p.pos();
        let mut v = p.vel();
        let mut a = s.gravity;
        if s.forces[0].is_some() || s.world_forces != 0 {
            let row = xf.get(entry.instance as usize).unwrap_or(&XformRow::IDENTITY);
            a += forces(s, x, row, time, attractors);
        }
        v = (v + a * dt) * drag[e];
        x += v * dt;
        p.pos_age = [x.x, x.y, x.z, age];
        p.vel_life = [v.x, v.y, v.z, life];
    }
}

/// Emit the particles of one spawn record into their ring slots; returns how many replaced a
/// particle that was still alive.
fn emit(pool: &mut [Particle], r: &SpawnRecord, dt: f32, entries: &[Entry], xf: &[XformRow]) -> u64 {
    let Some(e) = entries.get(r.entry as usize) else {
        return 0;
    };
    let mut overwrites = 0;
    let row = xf.get(e.instance as usize).unwrap_or(&XformRow::IDENTITY);
    let cap = e.capacity.max(1) as u64;
    let (t0, t1) = (subtick_q(r.t0), subtick_q(r.t1));
    let tick_step = dt * (1.0 / SUBTICKS as f32);
    for k in 0..r.count {
        let index = r.first + k as u64;
        let slot = e.base as usize + (index % cap) as usize;
        if slot >= pool.len() {
            continue;
        }
        let old = &pool[slot];
        if old.vel_life[3] != 0.0 && old.misc[2] == r.entry && old.pos_age[3] < old.vel_life[3] {
            overwrites += 1;
        }
        let fq = subtick(t0, t1, k, r.count);
        pool[slot] = spawn_particle(e, r.entry, row, index, fq, tick_step, r.extra_age);
    }
    overwrites
}

/// Particle `index` of layer instance `e` (entry `entry`), emitted at sub-tick `fq` of the
/// emitter motion `row` (`prev_*` → current pose), aged `(SUBTICKS − fq) × tick_step +
/// extra_age` minus its delay (pre-aged under gravity and drag). The GPU `emit` kernel computes
/// the same particle.
fn spawn_particle(
    e: &Entry,
    entry: u32,
    row: &XformRow,
    index: u64,
    fq: u32,
    tick_step: f32,
    extra_age: f32,
) -> Particle {
    let s = &e.sim;
    let flat = |v: Vec3| v - s.flatten_axis * (v.dot(s.flatten_axis) * s.flatten);
    let seed = particle_seed(e.seed, index);
    let f = fq as f32 * (1.0 / SUBTICKS as f32);
    let (origin, rot) = if s.local {
        (Vec3::ZERO, Quat::IDENTITY)
    } else {
        (row.prev_pos.lerp(row.pos, f), row.prev_rot.lerp(row.rot, f))
    };
    let scale = row.scale;
    let (offset, outward) = sample_shape(s.shape, seed, scale);
    let offset = flat(offset + s.offset * scale);
    let mut dir = match s.direction {
        EmitDirection::Shape => outward,
        EmitDirection::Fixed(d) => d.normalize_or(Vec3::NEG_Z),
        EmitDirection::Random => uniform_dir(rnd(seed, stream::DIR_A), rnd(seed, stream::DIR_B)),
        EmitDirection::Inward => -outward,
        EmitDirection::Tangent => Vec3::Y.cross(outward).try_normalize().unwrap_or(Vec3::X),
    };
    if s.flatten > 0.0 {
        dir = flat(dir).try_normalize().unwrap_or(dir);
    }
    if s.spread > 0.0 {
        dir = cone_dir(dir, s.spread, rnd(seed, stream::SPREAD_A), rnd(seed, stream::SPREAD_B));
    }
    // `pow` only for a non-zero exponent (`x⁰` is 1 anyway), like the GPU kernel.
    let speed_mul = if s.speed_exponent != 0.0 {
        scale.max(0.0).powf(s.speed_exponent)
    } else {
        1.0
    };
    let speed = range(seed, stream::SPEED, s.speed.0, s.speed.1) * speed_mul;
    let mut pos = origin + rot * offset;
    let mut vel = rot * dir * speed;
    if !s.local {
        vel += row.velocity * s.inherit;
    }
    let life = range_exact(seed, stream::LIFETIME, s.lifetime.0, s.lifetime.1);
    let delay = range_exact(seed, stream::DELAY, s.delay.0, s.delay.1);
    // Exact on both backends: an integer times a power-of-two multiple of `dt`, then two
    // sums (the GPU `emit` kernel evaluates the same expression).
    let age = (SUBTICKS - fq) as f32 * tick_step + extra_age - delay;
    if age > 0.0 {
        (pos, vel) = pre_age(pos, vel, s.gravity, s.drag, age);
    }
    let (x_axis, y_axis) = if matches!(s.facing, Facing::EmitterPlane) {
        (oct_encode(rot * Vec3::X), oct_encode(rot * Vec3::Y))
    } else {
        (0, 0)
    };
    Particle {
        pos_age: [pos.x, pos.y, pos.z, age],
        vel_life: [vel.x, vel.y, vel.z, life.max(0.0)],
        misc: [x_axis, seed, entry, y_axis],
    }
}

/// The in-plane basis of a plane-facing sprite (the sprite shader's `s3dp_plane_basis`).
pub(crate) fn plane_basis(n: Vec3) -> (Vec3, Vec3) {
    let helper = if n.y.abs() > 0.99 { Vec3::X } else { Vec3::Y };
    let bx = helper.cross(n).normalize_or(Vec3::X);
    (bx, n.cross(bx))
}

/// What the pack needs to know about a layer instance's render layer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PackLayer {
    /// Render layer id (draw parameters, LUT rows).
    pub id: u32,
    /// All emitters of the layer draw in one call (unsorted additive layers).
    pub batched: bool,
    /// Half extent of a sprite per unit of size (size curve maximum × aspect / stretch / 2).
    pub extent: f32,
}

/// A draw of render instances: `count` instances from `first` (CPU backend), or at most `count`
/// from `first` with the instance count in the indirect args `args` (GPU backend).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PackItem {
    pub first: u32,
    pub count: u32,
    pub layer: u32,
    /// Allocation serial of the (first) layer instance: the deterministic tie-break.
    pub serial: u64,
    /// Culling bounds (exact on the CPU backend, conservative on the GPU backend).
    pub min: Vec3,
    pub max: Vec3,
    /// Where the item sorts by depth: the same on both backends and for any render cadence
    /// ([`Runtime::entry_center`]).
    pub center: Vec3,
    /// Index of the item's `DrawIndirectArgs` (GPU backend), `u32::MAX` for direct draws.
    pub args: u32,
}

/// `args` of a direct draw.
pub(crate) const DIRECT: u32 = u32::MAX;

/// Inputs of [`pack`].
pub(crate) struct PackInput<'a> {
    pub rt: &'a Runtime,
    pub pool: &'a [Particle],
    /// Render pose (position, rotation) of every instance (local-space layers).
    pub poses: &'a [(Vec3, Quat)],
    /// Seconds to extrapolate (`Time::alpha × dt`, 0 without extrapolation).
    pub extrapolate: f32,
}

/// Write the render instances of every live, visible layer instance (oldest → newest per ring)
/// into `out`, grouped into draw items, in (render layer, allocation serial) order. `keys` is
/// scratch.
pub(crate) fn pack(
    input: &PackInput<'_>,
    layer_of: &dyn Fn(&Entry) -> Option<PackLayer>,
    keys: &mut Vec<(u32, u64, u32)>,
    out: &mut Vec<ParticleInstance>,
    items: &mut Vec<PackItem>,
) {
    let rt = input.rt;
    keys.clear();
    out.clear();
    items.clear();
    for (i, e) in rt.entries.iter().enumerate() {
        if !e.live || rt.instances[e.instance as usize].hidden {
            continue;
        }
        if let Some(l) = layer_of(e) {
            keys.push((l.id, e.serial, i as u32));
        }
    }
    keys.sort_unstable();
    let mut k = 0;
    while k < keys.len() {
        let (id, serial, first_entry) = keys[k];
        let e0 = &rt.entries[first_entry as usize];
        let Some(layer) = layer_of(e0) else {
            k += 1;
            continue;
        };
        let end = if layer.batched {
            keys[k..]
                .iter()
                .position(|&(l, _, _)| l != id)
                .map_or(keys.len(), |n| k + n)
        } else {
            k + 1
        };
        let start = out.len() as u32;
        let mut bounds = Bounds::EMPTY;
        for &(_, _, ei) in &keys[k..end] {
            let e = &rt.entries[ei as usize];
            let pose = input
                .poses
                .get(e.instance as usize)
                .copied()
                .unwrap_or((Vec3::ZERO, Quat::IDENTITY));
            let inst = &rt.instances[e.instance as usize];
            pack_entry(input, ei, e, inst.tint, inst.scale, pose, out, &mut bounds);
        }
        let count = out.len() as u32 - start;
        if count > 0 {
            let r = Vec3::splat(bounds.radius * layer.extent);
            items.push(PackItem {
                first: start,
                count,
                layer: id,
                serial,
                min: bounds.min - r,
                max: bounds.max + r,
                center: rt.entry_center(e0, input.poses),
                args: DIRECT,
            });
        }
        k = end;
    }
}

/// Culling bounds of packed particles: positions and the largest sprite size.
struct Bounds {
    min: Vec3,
    max: Vec3,
    radius: f32,
}

impl Bounds {
    const EMPTY: Self = Self {
        min: Vec3::splat(f32::INFINITY),
        max: Vec3::splat(f32::NEG_INFINITY),
        radius: 0.0,
    };
}

#[allow(clippy::too_many_arguments)]
fn pack_entry(
    input: &PackInput<'_>,
    index: u32,
    e: &Entry,
    tint: Vec4,
    scale: f32,
    pose: (Vec3, Quat),
    out: &mut Vec<ParticleInstance>,
    bounds: &mut Bounds,
) {
    let cap = e.capacity as u64;
    let base = e.base as usize;
    let tint = if e.sim.tinted { tint } else { Vec4::ONE };
    for j in 0..cap {
        let slot = base + ((e.spawned + j) % cap) as usize;
        let Some(p) = input.pool.get(slot) else {
            continue;
        };
        // Dead, or a particle of the range's previous owner (dead too by construction).
        if p.vel_life[3] == 0.0 || p.misc[2] != index {
            continue;
        }
        pack_particle(&e.sim, p, input.extrapolate, tint, scale, pose, out, bounds);
    }
}

/// The render instance of live particle `p` of a layer `s`, `dt` seconds after its simulated
/// state (extrapolation), if it is visible then (born, not dead).
#[allow(clippy::too_many_arguments)]
fn pack_particle(
    s: &LayerSim,
    p: &Particle,
    dt: f32,
    tint: Vec4,
    scale: f32,
    (pose_pos, pose_rot): (Vec3, Quat),
    out: &mut Vec<ParticleInstance>,
    bounds: &mut Bounds,
) {
    let life = p.vel_life[3];
    let age = p.pos_age[3] + dt;
    if age < 0.0 || age >= life {
        return;
    }
    let t = age / life;
    let seed = p.misc[1];
    let moving = if p.pos_age[3] >= 0.0 { dt } else { age.max(0.0) };
    let v = p.vel();
    let mut pos = p.pos() + v * moving;
    let mut vel = v;
    if s.local {
        pos = pose_pos + pose_rot * pos;
        vel = pose_rot * vel;
    }
    let mut size = range(seed, stream::SIZE, s.size.0, s.size.1) * scale;
    bounds.radius = bounds.radius.max(size.abs());
    if s.mirror && rnd(seed, stream::MIRROR) < 0.5 {
        size = -size;
    }
    let mut rot = range(seed, stream::ROTATION, s.rotation.0, s.rotation.1)
        + range(seed, stream::ANGULAR, s.angular.0, s.angular.1) * age;
    let dir = match s.facing {
        Facing::Plane { normal } => {
            if s.local {
                pose_rot * normal
            } else {
                normal
            }
        }
        Facing::EmitterPlane => {
            let (mut x, mut n) = (oct_decode(p.misc[0]), oct_decode(p.misc[3]));
            if s.local {
                x = pose_rot * x;
                n = pose_rot * n;
            }
            let (bx, by) = plane_basis(n);
            rot += x.dot(by).atan2(x.dot(bx));
            n
        }
        _ => vel,
    };
    let start = s.color_a.lerp(s.color_b, rnd(seed, stream::COLOR));
    let mut rgb = tint.truncate() * start.truncate();
    if let Some(fl) = s.flicker {
        let phase = rnd(seed, stream::FLICKER);
        let wave = 0.5 + 0.5 * (TAU * (fl.frequency * age + phase)).sin();
        rgb *= 1.0 - fl.amplitude.clamp(0.0, 1.0) * wave;
    }
    let alpha = tint.w * start.w;
    let a = if s.blend == ParticleBlend::Premultiplied {
        rgb *= alpha;
        range(seed, stream::COVERAGE, s.coverage.0, s.coverage.1) * alpha
    } else {
        alpha
    };
    rot -= TAU * (rot / TAU).round();
    bounds.min = bounds.min.min(pos);
    bounds.max = bounds.max.max(pos);
    out.push(ParticleInstance {
        pos_size: [pos.x, pos.y, pos.z, size],
        vel_age: [f16_bits(dir.x), f16_bits(dir.y), f16_bits(dir.z), f16_bits(t)],
        tint: [f16_bits(rgb.x), f16_bits(rgb.y), f16_bits(rgb.z), f16_bits(a)],
        rot_frame: [rot.to_bits(), pcg(seed.wrapping_add(stream::BITS.wrapping_mul(GOLDEN)))],
    });
}

/// Inputs of [`pack_bridges`].
pub(crate) struct BridgeInput<'a> {
    pub rt: &'a Runtime,
    /// Render pose of every instance.
    pub poses: &'a [(Vec3, Quat)],
    /// Whether each instance's emitter moved since the last tick (its `Transform` chain gives
    /// another pose than the tick read).
    pub moved: &'a [bool],
    /// Tick length (seconds, × `time_scale`).
    pub dt: f32,
    /// Seconds from the last tick to the render time (`Time::alpha × dt`, 0 without
    /// extrapolation): the age of a bridge's oldest end.
    pub age: f32,
}

/// Append the render-time bridges ([`Runtime::bridge`]) of every live, visible world-space
/// layer instance whose emitter moved since the last tick to `out`, as direct draw items in
/// (render layer, allocation serial) order (one per render layer when batched); returns the
/// particles written. `keys` is scratch.
pub(crate) fn pack_bridges(
    input: &BridgeInput<'_>,
    layer_of: &dyn Fn(&Entry) -> Option<PackLayer>,
    keys: &mut Vec<(u32, u64, u32)>,
    out: &mut Vec<ParticleInstance>,
    items: &mut Vec<PackItem>,
) -> u32 {
    let rt = input.rt;
    keys.clear();
    for (i, e) in rt.entries.iter().enumerate() {
        let inst = e.instance as usize;
        if !e.live || e.sim.local || !input.moved.get(inst).copied().unwrap_or(false) || rt.instances[inst].hidden {
            continue;
        }
        if let Some(l) = layer_of(e) {
            keys.push((l.id, e.serial, i as u32));
        }
    }
    if keys.is_empty() {
        return 0;
    }
    keys.sort_unstable();
    let written = out.len();
    let age_step = input.age.max(0.0) * (1.0 / SUBTICKS as f32);
    let mut k = 0;
    while k < keys.len() {
        let (id, serial, first_entry) = keys[k];
        let e0 = &rt.entries[first_entry as usize];
        let Some(layer) = layer_of(e0) else {
            k += 1;
            continue;
        };
        let end = if layer.batched {
            keys[k..]
                .iter()
                .position(|&(l, _, _)| l != id)
                .map_or(keys.len(), |n| k + n)
        } else {
            k + 1
        };
        let start = out.len() as u32;
        let mut bounds = Bounds::EMPTY;
        for &(_, _, ei) in &keys[k..end] {
            let e = &rt.entries[ei as usize];
            let Some(b) = rt.bridge(e, input.poses, input.dt) else {
                continue;
            };
            let inst = &rt.instances[e.instance as usize];
            let tint = if e.sim.tinted { inst.tint } else { Vec4::ONE };
            let (t0, t1) = (subtick_q(b.t0), subtick_q(b.t1));
            for j in 0..b.count {
                let fq = subtick(t0, t1, j, b.count);
                let p = spawn_particle(e, ei, &b.row, b.first + j as u64, fq, age_step, 0.0);
                pack_particle(
                    &e.sim,
                    &p,
                    0.0,
                    tint,
                    inst.scale,
                    (b.row.pos, b.row.rot),
                    out,
                    &mut bounds,
                );
            }
        }
        let count = out.len() as u32 - start;
        if count > 0 {
            let r = Vec3::splat(bounds.radius * layer.extent);
            items.push(PackItem {
                first: start,
                count,
                layer: id,
                serial,
                min: bounds.min - r,
                max: bounds.max + r,
                center: rt.entry_center(e0, input.poses),
                args: DIRECT,
            });
        }
        k = end;
    }
    (out.len() - written) as u32
}
