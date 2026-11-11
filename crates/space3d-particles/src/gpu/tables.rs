//! GPU backend data layouts (design-particles §6), mirrored in `shaders/gpu_common.wgsl`,
//! `simulate.wgsl` and `compact.wgsl`: the entry table (a layer instance's simulation and
//! render parameters, 24 `vec4<u32>` words), jobs and groups of the compaction, spawn records,
//! emitter frames and the per-tick and per-frame uniforms.

use crate::effect::{EmitDirection, EmitShape, Facing, Falloff, Force, ParticleBlend};
use crate::runtime::{Entry, WorldAttractor, XformRow};
use space3d_core::glam::{Quat, Vec3, Vec4};

/// `vec4<u32>` words per entry.
pub(crate) const ENTRY_WORDS: usize = 24;

/// Entry flags (word 1.x); bits 16..19 hold the number of layer forces.
pub(crate) mod flags {
    pub const LOCAL: u32 = 1;
    pub const EMITTER_PLANE: u32 = 2;
    pub const MIRRORED: u32 = 4;
    pub const PREMULTIPLIED: u32 = 8;
    pub const TINTED: u32 = 16;
    pub const FLICKERS: u32 = 32;
    pub const PLANE: u32 = 64;
    /// Rotation and age in seconds as two halves (`LayerSim::pack_age`).
    pub const AGE: u32 = 128;
}

/// A table word.
pub(crate) type Word = [u32; 4];

fn f(v: [f32; 4]) -> Word {
    v.map(f32::to_bits)
}

fn v3(v: Vec3, w: f32) -> Word {
    f([v.x, v.y, v.z, w])
}

/// The 24 words of entry `e` (`s3dp_word(e, k)` in WGSL).
pub(crate) fn entry_words(e: &Entry, out: &mut [Word]) {
    let s = &e.sim;
    let mut fl = 0;
    if s.local {
        fl |= flags::LOCAL;
    }
    match s.facing {
        Facing::EmitterPlane => fl |= flags::EMITTER_PLANE,
        Facing::Plane { .. } => fl |= flags::PLANE,
        _ => {}
    }
    if s.mirror {
        fl |= flags::MIRRORED;
    }
    if s.blend == ParticleBlend::Premultiplied {
        fl |= flags::PREMULTIPLIED;
    }
    if s.tinted {
        fl |= flags::TINTED;
    }
    if s.flicker.is_some() {
        fl |= flags::FLICKERS;
    }
    if s.pack_age {
        fl |= flags::AGE;
    }
    let n_forces = s.forces.iter().take_while(|f| f.is_some()).count() as u32;
    fl |= n_forces << 16;
    let (shape, params) = match s.shape {
        EmitShape::Point => (0, [0.0; 4]),
        EmitShape::Sphere { radius, inner } => (1, [radius, inner, 0.0, 0.0]),
        EmitShape::Cone { angle, radius } => (2, [angle, radius, 0.0, 0.0]),
        EmitShape::Ring { radius, width, arc } => (3, [radius, width, arc, 0.0]),
        EmitShape::Box { half_extents: h } => (4, [h.x, h.y, h.z, 0.0]),
        EmitShape::Line { length } => (5, [length, 0.0, 0.0, 0.0]),
    };
    let (direction, fixed) = match s.direction {
        EmitDirection::Shape => (0, Vec3::ZERO),
        EmitDirection::Fixed(d) => (1, d),
        EmitDirection::Random => (2, Vec3::ZERO),
        EmitDirection::Inward => (3, Vec3::ZERO),
        EmitDirection::Tangent => (4, Vec3::ZERO),
    };
    out[0] = [e.base, e.capacity, e.seed, e.instance];
    out[1] = [fl, shape, direction, s.world_forces];
    out[2] = f(params);
    out[3] = v3(s.offset, s.spread);
    out[4] = v3(fixed, s.flatten);
    out[5] = v3(s.flatten_axis, s.speed_exponent);
    out[6] = f([s.lifetime.0, s.lifetime.1, s.delay.0, s.delay.1]);
    out[7] = f([s.speed.0, s.speed.1, s.inherit, s.drag]);
    out[8] = v3(s.gravity, 0.0);
    for (i, force) in s.forces.iter().enumerate() {
        let (a, b) = match force {
            Some(Force::Attractor {
                position,
                strength,
                radius,
                falloff,
            }) => (v3(*position, *strength), [radius.to_bits(), falloff_id(*falloff), 0, 0]),
            Some(Force::Vortex { axis, strength, pull }) => (v3(*axis, *strength), [pull.to_bits(), 0, 0, 1]),
            Some(Force::Turbulence {
                strength,
                frequency,
                speed,
            }) => (f([*strength, *frequency, *speed, 0.0]), [0, 0, 0, 2]),
            Some(Force::Curl {
                strength,
                frequency,
                speed,
            }) => (f([*strength, *frequency, *speed, 0.0]), [0, 0, 0, 3]),
            None => ([0; 4], [0; 4]),
        };
        out[9 + 2 * i] = a;
        out[10 + 2 * i] = b;
    }
    out[17] = f([s.size.0, s.size.1, s.rotation.0, s.rotation.1]);
    out[18] = f([s.angular.0, s.angular.1, s.coverage.0, s.coverage.1]);
    out[19] = f(s.color_a.to_array());
    out[20] = f(s.color_b.to_array());
    let (freq, amp) = s.flicker.map_or((0.0, 0.0), |fl| (fl.frequency, fl.amplitude));
    out[21] = f([freq, amp, s.spawn.max_rate_scale, s.throttle.speed]);
    let normal = match s.facing {
        Facing::Plane { normal } => normal,
        _ => Vec3::ZERO,
    };
    out[22] = v3(normal, 0.0);
    out[23] = [0; 4];
}

pub(crate) fn falloff_id(f: Falloff) -> u32 {
    match f {
        Falloff::Linear => 1,
        Falloff::InverseSquare => 2,
        _ => 0,
    }
}

/// The four words of a compaction job: (entry, tail, first record, group), the instance tint,
/// (render position, scale), render rotation.
pub(crate) fn job_words(
    entry: u32,
    tail: u32,
    record: u32,
    group: u32,
    tint: Vec4,
    pose: (Vec3, Quat),
    scale: f32,
) -> [Word; 4] {
    [
        [entry, tail, record, group],
        f(tint.to_array()),
        v3(pose.0, scale),
        f(pose.1.to_array()),
    ]
}

/// An emitter frame (`S3dpXform`, 80 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct GpuXform {
    pub prev_pos: [f32; 4],
    pub prev_rot: [f32; 4],
    pub pos: [f32; 4],
    pub rot: [f32; 4],
    pub vel: [f32; 4],
}

impl GpuXform {
    pub const IDENTITY: Self = Self {
        prev_pos: [0.0, 0.0, 0.0, 1.0],
        prev_rot: [0.0, 0.0, 0.0, 1.0],
        pos: [0.0; 4],
        rot: [0.0, 0.0, 0.0, 1.0],
        vel: [0.0; 4],
    };

    pub fn new(r: &XformRow) -> Self {
        let p = |v: Vec3, w: f32| [v.x, v.y, v.z, w];
        Self {
            prev_pos: p(r.prev_pos, r.scale),
            prev_rot: r.prev_rot.to_array(),
            pos: p(r.pos, 0.0),
            rot: r.rot.to_array(),
            vel: p(r.velocity, r.throttle),
        }
    }
}

/// A spawn record (`S3dpSpawn`, 48 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct GpuSpawn {
    pub a: [u32; 4],
    pub b: [u32; 4],
    pub c: [u32; 4],
}

/// One replayed tick (`S3dpTick`, 304 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct GpuTick {
    pub a: [f32; 4],
    pub b: [u32; 4],
    pub c: [u32; 4],
    pub attractors: [[f32; 4]; 16],
}

impl GpuTick {
    /// The uniform of a tick of `dt` seconds (`substeps` substeps) starting at runtime `time`.
    pub fn new(dt: f32, substeps: u32, time: f64, attractors: &[WorldAttractor]) -> Self {
        let substeps = substeps.max(1);
        let mut out = Self {
            a: [
                dt,
                dt / substeps as f32,
                time as f32,
                dt * (1.0 / crate::cpu::SUBTICKS as f32),
            ],
            b: [substeps, 0, 0, 0],
            c: [0, 0, attractors.len().min(8) as u32, 0],
            attractors: [[0.0; 4]; 16],
        };
        for (i, w) in attractors.iter().take(8).enumerate() {
            out.attractors[2 * i] = [w.position.x, w.position.y, w.position.z, w.strength];
            out.attractors[2 * i + 1] = [
                w.radius,
                f32::from_bits(falloff_id(w.falloff)),
                f32::from_bits(w.mask),
                0.0,
            ];
        }
        out
    }
}

/// Frame parameters of the compaction (`S3dpFrame`, 64 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct GpuFrame {
    pub a: [u32; 4],
    pub b: [u32; 4],
    pub c: [f32; 4],
    pub d: [u32; 4],
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_match_the_shaders() {
        assert_eq!(std::mem::size_of::<GpuXform>(), 80);
        assert_eq!(std::mem::size_of::<GpuSpawn>(), 48);
        assert_eq!(std::mem::size_of::<GpuTick>(), 304);
        assert_eq!(std::mem::size_of::<GpuFrame>(), 64);
        assert_eq!(std::mem::size_of::<crate::cpu::Particle>(), 48);
    }
}
