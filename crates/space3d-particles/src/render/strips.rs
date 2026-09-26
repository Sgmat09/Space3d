//! Strips: the ribbons (from the runtime's ribbon tracks) and beams (from the emitters' beam
//! layers and `ParticleBeam` ends) of a frame, as one stream of [`StripPoint`]s built on the CPU
//! at render time (both backends) and drawn by `strip.wgsl` (design-particles §8.5).
//!
//! A ribbon is its track's live points, oldest first — with one dead point before them, which
//! the shader clips to where the age reaches the life, so the tail shortens smoothly — and a
//! head at the emitter's render pose. A beam is `segments + 1` points from the emitter's render
//! pose to its end, with jitter, fade and flicker, plus optional round glows at its ends. The
//! additive strips of a layer share one draw (a point flagged `START` begins a new strip);
//! others draw one strip each, sorted like emitters.

use super::layers::{RenderLayers, tile_length};
use crate::components::{BeamEnd, ParticleBeam};
use crate::cpu::{DIRECT, PackItem};
use crate::effect::{BeamLayer, RibbonUv};
use crate::pack::f16_bits;
use crate::registry::ParticleEffects;
use crate::rng::{mix, pcg, rnd};
use crate::runtime::{Instance, Runtime, Track};
use space3d_core::glam::{Quat, Vec3, Vec4};
use space3d_core::{GlobalTransform, World};

/// A point of a strip (40 bytes, per-instance vertex attributes, read twice per segment).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct StripPoint {
    /// `Float32x4`: world position, u.
    pub pos_u: [f32; 4],
    /// `Float16x4`: tangent, t (the LUT coordinate).
    pub tan_t: [u16; 4],
    /// `Float16x4`: linear tint (× fade, flicker), alpha.
    pub tint: [u16; 4],
    /// `Uint32x2`: x = flags | width multiplier (f16) << 16, y = random bits.
    pub misc: [u32; 2],
}

/// Bytes per strip point.
pub(crate) const POINT_SIZE: u64 = std::mem::size_of::<StripPoint>() as u64;

/// The point begins a strip (the segment ending at it is not drawn).
pub(crate) const START: u32 = 1;
/// The strip has a round end at the point.
pub(crate) const CAP: u32 = 2;
/// `t` runs along the strip (beams): no point is dead.
pub(crate) const ALONG: u32 = 4;

/// Strip `u` is kept small by whole multiples of this (the shader's noise repeats every 128
/// units of `u` — 256 cells at 2 per unit, 512 at 4 — textures every 1).
const U_PERIOD: f64 = 128.0;

/// Inputs of [`build`].
pub(crate) struct StripInput<'a> {
    pub rt: &'a Runtime,
    pub effects: &'a ParticleEffects,
    pub layers: &'a RenderLayers,
    /// Render pose of every instance.
    pub poses: &'a [(Vec3, Quat)],
    pub world: &'a World,
    /// Seconds from the last tick to the render time (`Time::alpha × dt`).
    pub extrapolate: f32,
    /// Most points.
    pub budget: u32,
}

/// What a strip is built from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Source {
    Track(u32),
    Beam { instance: u32, beam: u16 },
}

/// Scratch of [`build`] (reused, never shrunk).
#[derive(Debug, Default)]
pub(crate) struct StripScratch {
    keys: Vec<(u32, u64, Source)>,
    pub warned_budget: bool,
}

impl StripScratch {
    pub fn capacity(&self) -> usize {
        self.keys.capacity()
    }
}

fn beam_visible(inst: &Instance) -> bool {
    inst.live && inst.attached && !inst.orphaned && !inst.hidden && !inst.finished && inst.is_emitting()
}

/// Build the frame's strips into `out` and their draw items into `items` (appended); returns
/// the segments drawn.
pub(crate) fn build(
    input: &StripInput<'_>,
    scratch: &mut StripScratch,
    out: &mut Vec<StripPoint>,
    items: &mut Vec<PackItem>,
) -> u32 {
    out.clear();
    let rt = input.rt;
    let keys = &mut scratch.keys;
    keys.clear();
    for (k, tr) in rt.tracks.iter().enumerate() {
        if !tr.live || tr.len == 0 {
            continue;
        }
        let inst = &rt.instances[tr.instance as usize];
        if inst.hidden {
            continue;
        }
        if let Some(l) = input.layers.ribbon_layer(tr.effect, tr.layer) {
            keys.push((l.pack.id, tr.serial, Source::Track(k as u32)));
        }
    }
    for (i, inst) in rt.instances.iter().enumerate() {
        if !beam_visible(inst) {
            continue;
        }
        let Some(effect) = input.effects.get(inst.effect) else {
            continue;
        };
        for b in 0..effect.beams.len() as u16 {
            if let Some(l) = input.layers.beam_layer(inst.effect, b) {
                let serial = inst.entity.to_bits().get();
                keys.push((
                    l.pack.id,
                    serial,
                    Source::Beam {
                        instance: i as u32,
                        beam: b,
                    },
                ));
            }
        }
    }
    if keys.is_empty() {
        return 0;
    }
    keys.sort_unstable();
    let now = rt.time + input.extrapolate.max(0.0) as f64;
    let mut segments = 0u32;
    let mut k = 0;
    while k < keys.len() {
        let (id, serial, _) = keys[k];
        let info = input.layers.info[id as usize];
        let end = if info.pack.batched {
            keys[k..]
                .iter()
                .position(|&(l, _, _)| l != id)
                .map_or(keys.len(), |n| k + n)
        } else {
            k + 1
        };
        let first = out.len();
        let mut bounds = (Vec3::splat(f32::INFINITY), Vec3::splat(f32::NEG_INFINITY), 0.0f32);
        for &(_, _, source) in &keys[k..end] {
            let start = out.len();
            match source {
                Source::Track(t) => ribbon(input, &rt.tracks[t as usize], now, out),
                Source::Beam { instance, beam } => beam_strip(input, instance, beam, now, out),
            }
            let n = out.len() - start;
            if n < 2 || out.len() > input.budget as usize {
                if out.len() > input.budget as usize && !scratch.warned_budget {
                    scratch.warned_budget = true;
                    tracing::warn!(
                        budget = input.budget,
                        "ribbon and beam points exceed ParticleSettings::max_ribbon_segments; some are not drawn"
                    );
                }
                out.truncate(start);
                continue;
            }
            segments += n as u32 - 1;
            for p in &out[start..] {
                let pos = Vec3::new(p.pos_u[0], p.pos_u[1], p.pos_u[2]);
                bounds.0 = bounds.0.min(pos);
                bounds.1 = bounds.1.max(pos);
                bounds.2 = bounds.2.max(crate::pack::f16_value((p.misc[0] >> 16) as u16));
            }
        }
        let n = (out.len() - first) as u32;
        if n >= 2 {
            let r = Vec3::splat(2.0 * info.pack.extent * bounds.2.max(0.0) + 1e-3);
            let (min, max) = (bounds.0 - r, bounds.1 + r);
            items.push(PackItem {
                first: first as u32,
                count: n - 1,
                layer: id,
                serial,
                min,
                max,
                center: (min + max) * 0.5,
                args: DIRECT,
            });
        }
        k = end;
    }
    segments
}

fn point(pos: Vec3, u: f64, t: f32, tint: Vec4, flags: u32, width: f32, bits: u32) -> StripPoint {
    StripPoint {
        pos_u: [pos.x, pos.y, pos.z, u as f32],
        tan_t: [0, 0, 0, f16_bits(t)],
        tint: [f16_bits(tint.x), f16_bits(tint.y), f16_bits(tint.z), f16_bits(tint.w)],
        misc: [flags | (f16_bits(width) as u32) << 16, bits],
    }
}

fn pos_of(p: &StripPoint) -> Vec3 {
    Vec3::new(p.pos_u[0], p.pos_u[1], p.pos_u[2])
}

/// Tangents of the strip `points` (central differences, one-sided at the ends; a zero-length
/// neighborhood keeps its neighbor's), and `u` rebased by whole periods.
fn finish_strip(points: &mut [StripPoint], fallback: Vec3) {
    let n = points.len();
    let base = (points[0].pos_u[3] as f64 / U_PERIOD).floor() * U_PERIOD;
    let mut last = fallback;
    for j in 0..n {
        let a = pos_of(&points[j.saturating_sub(1)]);
        let b = pos_of(&points[(j + 1).min(n - 1)]);
        let t = (b - a).try_normalize().unwrap_or(last);
        last = t;
        points[j].tan_t[0] = f16_bits(t.x);
        points[j].tan_t[1] = f16_bits(t.y);
        points[j].tan_t[2] = f16_bits(t.z);
        points[j].pos_u[3] = (points[j].pos_u[3] as f64 - base) as f32;
    }
}

/// `u` at distance `dist` along a strip (`Tile`) or `stretched` (0..1, `Stretch`) at render time
/// `now`: the pattern (texture, noise) moves toward increasing `u` — a beam's end, a trail's
/// head — at `scroll` units of `u` per second.
fn strip_u(uv: RibbonUv, dist: f64, stretched: f32, scroll: f32, now: f64) -> f64 {
    let u = match tile_length(uv) {
        Some(len) => dist / len as f64,
        None => stretched as f64,
    };
    u - scroll as f64 * now
}

fn ribbon(input: &StripInput<'_>, tr: &Track, now: f64, out: &mut Vec<StripPoint>) {
    let rt = input.rt;
    let Some(effect) = input.effects.get(tr.effect) else {
        return;
    };
    let Some(layer) = effect.ribbons.get(tr.layer as usize) else {
        return;
    };
    let inst = &rt.instances[tr.instance as usize];
    let life = tr.sim.lifetime as f64;
    // The first live point, and the dead one before it (clipped by the shader).
    let mut first = tr.len;
    for j in 0..tr.len {
        if now - rt.points[tr.slot(j)].time < life {
            first = j;
            break;
        }
    }
    let from = first.saturating_sub(1);
    let start = out.len();
    let bits = pcg(tr.serial as u32 ^ 0x51d0_7a11);
    let width = inst.scale.abs();
    let mut newest = None;
    for j in from..tr.len {
        let p = &rt.points[tr.slot(j)];
        let t = ((now - p.time) / life) as f32;
        let flags = if out.len() == start { START } else { 0 };
        out.push(point(
            p.pos,
            strip_u(layer.uv, p.dist, 1.0 - t.min(1.0), layer.scroll, now),
            t.max(0.0),
            p.tint,
            flags,
            width,
            bits,
        ));
        newest = Some(p);
    }
    // The head at the emitter's render pose, while it emits.
    let head_pose = input.poses.get(tr.instance as usize).copied();
    if let (Some(last), Some((pos, rot))) = (newest, head_pose)
        && inst.emitting_at(tr.sim.start, 0.0)
    {
        let head = pos + rot * (tr.sim.offset * inst.scale);
        let d = head.distance(last.pos);
        if d > 1e-4 {
            let tint = if tr.sim.tinted { inst.tint } else { Vec4::ONE };
            out.push(point(
                head,
                strip_u(layer.uv, last.dist + d as f64, 1.0, layer.scroll, now),
                0.0,
                tint,
                CAP,
                width,
                bits,
            ));
        } else if let Some(p) = out.last_mut() {
            p.misc[0] |= CAP;
        }
    } else if out.len() > start
        && let Some(p) = out.last_mut()
    {
        // A drained trail (the emitter stopped): its newest point is its round end.
        p.misc[0] |= CAP;
    }
    if out.len() - start >= 2 {
        finish_strip(&mut out[start..], Vec3::X);
    }
}

/// The end of beam layers of `inst` (render pose `pose`): its `ParticleBeam`, else `length`
/// along the emitter's −Z. `None` when the end entity has no transform.
fn beam_end(input: &StripInput<'_>, inst: &Instance, pose: (Vec3, Quat), length: f32) -> Option<(Vec3, f32)> {
    match input.world.get::<&ParticleBeam>(inst.entity).ok().map(|b| *b) {
        Some(b) => {
            let end = match b.end {
                BeamEnd::World(p) => p,
                BeamEnd::Local(p) => pose.0 + pose.1 * p,
                BeamEnd::Entity(e) => input.world.get::<&GlobalTransform>(e).ok()?.0.w_axis.truncate(),
            };
            Some((end, b.width_scale))
        }
        None => Some((pose.0 + pose.1 * Vec3::new(0.0, 0.0, -length * inst.scale), 1.0)),
    }
}

fn beam_strip(input: &StripInput<'_>, i: u32, b: u16, now: f64, out: &mut Vec<StripPoint>) {
    let rt = input.rt;
    let inst = &rt.instances[i as usize];
    let Some(effect) = input.effects.get(inst.effect) else {
        return;
    };
    let Some(layer) = effect.beams.get(b as usize) else {
        return;
    };
    let Some(&pose) = input.poses.get(i as usize) else {
        return;
    };
    let Some((end, width_scale)) = beam_end(input, inst, pose, layer.length) else {
        return;
    };
    let start = pose.0 + pose.1 * (layer.offset * inst.scale);
    let seed = mix(inst.seed, 0xbea0 + b as u32);
    let tint = beam_tint(layer, inst, seed, now, input.extrapolate.max(0.0));
    let width = width_scale * inst.scale.abs();
    let dir = (end - start).try_normalize().unwrap_or(pose.1 * Vec3::NEG_Z);
    let length = start.distance(end) as f64;
    let (bx, by) = dir.any_orthonormal_pair();
    let segs = layer.segments.clamp(1, crate::effect::MAX_BEAM_SEGMENTS);
    let frame = if layer.jitter_hz > 0.0 {
        (now * layer.jitter_hz as f64).floor() as u32
    } else {
        0
    };
    let first = out.len();
    for j in 0..=segs {
        let f = j as f32 / segs as f32;
        let mut p = start.lerp(end, f);
        if layer.jitter != 0.0 && j > 0 && j < segs {
            let key = pcg(seed ^ frame.wrapping_mul(0x9E37_79B9) ^ j);
            let env = (std::f32::consts::PI * f).sin() * layer.jitter * inst.scale;
            p += bx * ((rnd(key, 0) * 2.0 - 1.0) * env) + by * ((rnd(key, 1) * 2.0 - 1.0) * env);
        }
        let flags = ALONG
            | match j {
                0 => START | CAP,
                _ if j == segs => CAP,
                _ => 0,
            };
        let u = strip_u(layer.uv, length * f as f64, f, layer.scroll, now);
        out.push(point(p, u, f, tint, flags, width, seed));
    }
    finish_strip(&mut out[first..], dir);
    // Round glows at the ends: a zero-length pair of capped points each.
    for (glow, at, f) in [(layer.start_glow, start, 0.0), (layer.end_glow, end, 1.0)] {
        if glow > 0.0 {
            let g = out.len();
            let u = strip_u(layer.uv, length * f as f64, f, layer.scroll, now);
            out.push(point(at, u, f, tint, ALONG | START | CAP, width * glow, seed));
            out.push(point(at, u, f, tint, ALONG | CAP, width * glow, seed));
            finish_strip(&mut out[g..], dir);
            for p in &mut out[g..] {
                p.tan_t[0] = f16_bits(dir.x);
                p.tan_t[1] = f16_bits(dir.y);
                p.tan_t[2] = f16_bits(dir.z);
            }
        }
    }
}

/// A beam's tint: the emitter's (for tinted layers), its `fade` over the effect's time and its
/// flicker at render time `now`.
fn beam_tint(layer: &BeamLayer, inst: &Instance, seed: u32, now: f64, extrapolate: f32) -> Vec4 {
    let base = if layer.tinted { inst.tint } else { Vec4::ONE };
    let t = if inst.looping {
        0.0
    } else {
        ((inst.time as f32 + extrapolate) / inst.duration.max(1e-6)).clamp(0.0, 1.0)
    };
    let mut k = layer.fade.eval(t).max(0.0);
    if let Some(fl) = layer.flicker {
        let phase = rnd(seed, 2);
        let wave = 0.5 + 0.5 * (std::f64::consts::TAU * (fl.frequency as f64 * now + phase as f64)).sin() as f32;
        k *= 1.0 - fl.amplitude.clamp(0.0, 1.0) * wave;
    }
    (base.truncate() * k).extend(base.w)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::{EffectSpawn, SpawnEffect};
    use crate::registry::builtin;
    use crate::{ParticleEffects, ParticleSettings};
    use space3d_core::{Entity, Transform};

    const DT: f32 = 1.0 / 60.0;

    fn median_ms(n: usize, mut f: impl FnMut()) -> f64 {
        let mut samples: Vec<f64> = (0..n)
            .map(|_| {
                let t = std::time::Instant::now();
                f();
                t.elapsed().as_secs_f64() * 1e3
            })
            .collect();
        samples.sort_by(f64::total_cmp);
        samples[n / 2]
    }

    /// The strips of a Battle frame, built on the CPU every frame (both backends): 20 bolts'
    /// `LASER_TRAIL` ribbons at 1500 units/s, 10 missiles' `MISSILE_TRAIL` (smoke and wake
    /// ribbons) at 420 units/s, 10 ships' `ENGINE_AFTERBURNER` trails, 6 `LASER_BEAM`s and 4
    /// `REPAIR_BEAM`s (jittered filaments) — about 1 900 points — within 0.1 ms (Dev machine,
    /// release; ×4 in the test profile, `SPACE3D_PERF_BUDGET_SCALE` elsewhere).
    #[test]
    fn strips_of_a_battle_within_budget() {
        let mut world = World::new();
        let mut effects = ParticleEffects::default();
        for h in builtin::ALL {
            effects.set_builtin(h, builtin::effect(h).unwrap());
        }
        let settings = ParticleSettings::default();
        let mut rt = Runtime::default();
        let mut movers: Vec<(Entity, Vec3, Vec3)> = Vec::new();
        for i in 0..20 {
            let p = Vec3::new(-800.0, 0.0, i as f32 * 20.0);
            let e = world.spawn_effect(builtin::LASER_TRAIL, EffectSpawn::at(p).seed(i));
            movers.push((e, p, Vec3::X * 1500.0));
        }
        for i in 0..10 {
            let p = Vec3::new(-400.0, 0.0, 500.0 + i as f32 * 30.0);
            let e = world.spawn_effect(builtin::MISSILE_TRAIL, EffectSpawn::at(p).seed(100 + i));
            movers.push((e, p, Vec3::new(420.0, 0.0, -60.0)));
        }
        for i in 0..10 {
            let p = Vec3::new(0.0, 0.0, -500.0 - i as f32 * 60.0);
            let e = world.spawn_effect(builtin::ENGINE_AFTERBURNER, EffectSpawn::at(p).seed(200 + i));
            movers.push((e, p, Vec3::new(0.0, 0.0, -250.0)));
        }
        for i in 0..10u32 {
            let fx = if i < 6 {
                builtin::LASER_BEAM
            } else {
                builtin::REPAIR_BEAM
            };
            let p = Vec3::new(i as f32 * 50.0, 0.0, 0.0);
            world.spawn_effect(
                fx,
                EffectSpawn::at(p)
                    .beam_to(BeamEnd::World(p + Vec3::new(40.0, 0.0, 300.0)))
                    .seed(300 + i),
            );
        }
        for t in 1..=90 {
            for &(e, p, v) in &movers {
                world.get::<&mut Transform>(e).unwrap().translation = p + v * (t as f32 * DT);
            }
            rt.tick(&world, &settings, &effects, DT);
            rt.log.clear();
        }
        let mut layers = RenderLayers::default();
        layers.sync(&effects);
        let poses: Vec<(Vec3, Quat)> = rt.instances.iter().map(|i| (i.pos, i.rot)).collect();
        let input = StripInput {
            rt: &rt,
            effects: &effects,
            layers: &layers,
            poses: &poses,
            world: &world,
            extrapolate: 0.5 * DT,
            budget: settings.max_ribbon_segments,
        };
        let (mut scratch, mut out, mut items) = (StripScratch::default(), Vec::new(), Vec::new());
        let mut segments = 0;
        let ms = median_ms(100, || {
            items.clear();
            segments = build(&input, &mut scratch, &mut out, &mut items);
        });
        let scale = std::env::var("SPACE3D_PERF_BUDGET_SCALE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1.0)
            * if cfg!(debug_assertions) { 4.0 } else { 1.0 };
        let budget = 0.1 * scale;
        eprintln!(
            "strips: {} points, {segments} segments, {} draws in {:.1} µs (budget {:.0} µs)",
            out.len(),
            items.len(),
            ms * 1e3,
            budget * 1e3
        );
        assert!(out.len() > 1500, "{} points", out.len());
        assert!(ms <= budget, "{ms:.4} ms > {budget:.3} ms");
        // Deterministic: the same stream again.
        let first = out.clone();
        items.clear();
        build(&input, &mut scratch, &mut out, &mut items);
        assert_eq!(first, out);
    }
}
