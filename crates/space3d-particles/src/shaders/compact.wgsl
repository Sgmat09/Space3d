// Space3d particles, GPU backend: compaction into the render instance stream (design-particles
// §7 steps 3–5), the GPU twin of `cpu::pack`. Composed after `gpu_common.wgsl`.
//
// A *job* is a layer instance to draw (its ring read oldest → newest from `tail`), a *group* a
// draw (one job, or every job of a batched additive layer), a *record* 64 ring positions of one
// job (one workgroup of `mark` and `scatter`), a *block* 64 consecutive records of a group.
// * `mark`: a thread per ring position; a workgroup builds the 64-bit mask of its visible
//   particles (`atomicOr` in workgroup memory, order independent) and their count, and adds the
//   count to its block's total and to its group's `DrawIndirectArgs::instance_count` (sums of
//   integers: the same whatever the order).
// * `scatter`: a record's output offset is the sum of its group's earlier blocks' totals and of
//   its block's earlier records' counts — one parallel load per lane and a workgroup sum (the
//   prefix scan in two levels, without a pass of its own) — and each visible particle writes
//   its instance at `group out + prefix + bits below its lane`.
// Instance order is therefore the CPU pack's (jobs in draw order, rings oldest first), and no
// output position depends on atomic ordering.

// Frame parameters (`gpu::tables::GpuFrame`).
struct S3dpFrame {
    // x: first job word, y: jobs, z: first group word, w: groups.
    a: vec4<u32>,
    // x: records; offsets in `work`: y the groups' draw arguments, z counts, w block totals.
    b: vec4<u32>,
    // x: seconds to extrapolate (`Time::alpha × dt`), yzw: 0.
    c: vec4<f32>,
    // x: first record word in `tables`, yzw: 0.
    d: vec4<u32>,
}

@group(0) @binding(0) var<storage, read> pool: array<S3dpParticle>;
// Per record its 64-bit mask (2 words), then the records' counts, the blocks' totals and the
// groups' `DrawIndirectArgs` (4 words each; the CPU resets totals and arguments every frame).
@group(0) @binding(2) var<storage, read_write> work: array<atomic<u32>>;
// `cpu::ParticleInstance`, 10 words each.
@group(0) @binding(3) var<storage, read_write> instances: array<u32>;
@group(0) @binding(4) var<uniform> frame: S3dpFrame;

var<workgroup> s3dp_mask: array<atomic<u32>, 2>;
var<workgroup> s3dp_prefix: atomic<u32>;

// Job words: (entry, tail, first record, group), tint, (render position, scale), render
// rotation.
fn s3dp_job(j: u32, k: u32) -> vec4<u32> {
    return tables[frame.a.x + j * 4u + k];
}

// Group words: (first record, records, first instance, first block).
fn s3dp_group(g: u32) -> vec4<u32> {
    return tables[frame.a.z + g];
}

// Record `r` (written by the CPU, so a workgroup finds its ring positions with one load):
// x: job | lanes in use << 24, y: pool slot of lane 0, z: ring start, w: ring capacity.
fn s3dp_record(r: u32) -> vec4<u32> {
    return tables[frame.d.x + r];
}

// Pool slot of lane `lane` of a record, or `0xffffffff` past the ring's end.
fn s3dp_slot(rec: vec4<u32>, lane: u32) -> u32 {
    if (lane >= (rec.x >> 24u)) {
        return 0xffffffffu;
    }
    let slot = rec.y + lane;
    return select(slot, slot - rec.w, slot >= rec.z + rec.w);
}

@compute @workgroup_size(64)
fn mark(
    @builtin(workgroup_id) wid: vec3<u32>,
    @builtin(num_workgroups) groups: vec3<u32>,
    @builtin(local_invocation_index) lane: u32,
) {
    let r = wid.x + wid.y * groups.x;
    if (r >= frame.b.x) {
        return;
    }
    if (lane < 2u) {
        atomicStore(&s3dp_mask[lane], 0u);
    }
    workgroupBarrier();
    let rec = s3dp_record(r);
    let slot = s3dp_slot(rec, lane);
    let job = s3dp_job(rec.x & 0xffffffu, 0u);
    if (slot != 0xffffffffu) {
        let life = pool[slot].vel_life.w;
        let age = pool[slot].pos_age.w + frame.c.x;
        if (life != 0.0 && pool[slot].misc.z == job.x && age >= 0.0 && age < life) {
            atomicOr(&s3dp_mask[lane >> 5u], 1u << (lane & 31u));
        }
    }
    workgroupBarrier();
    if (lane == 0u) {
        let lo = atomicLoad(&s3dp_mask[0]);
        let hi = atomicLoad(&s3dp_mask[1]);
        let count = countOneBits(lo) + countOneBits(hi);
        atomicStore(&work[2u * r], lo);
        atomicStore(&work[2u * r + 1u], hi);
        atomicStore(&work[frame.b.z + r], count);
        if (count > 0u) {
            let group = s3dp_group(job.w);
            atomicAdd(&work[frame.b.w + group.w + (r - group.x) / 64u], count);
            atomicAdd(&work[frame.b.y + 4u * job.w + 1u], count);
        }
    }
}

// `pack::f16_bits` in integers (bit-identical; `pack2x16float` needs a capability llvmpipe
// lacks): round to nearest even, clamped to ±65504, NaN → 0.
fn s3dp_f16(x: f32) -> u32 {
    if ((bitcast<u32>(x) & 0x7fffffffu) > 0x7f800000u) {
        return 0u;
    }
    let bits = bitcast<u32>(clamp(x, -65504.0, 65504.0));
    let sign = (bits >> 16u) & 0x8000u;
    let e = i32((bits >> 23u) & 0xffu) - 112;
    let man = bits & 0x7fffffu;
    if (e <= 0) {
        if (e < -10) {
            return sign;
        }
        let m = man | 0x800000u;
        let shift = u32(14 - e);
        let half = m >> shift;
        let rem = m & ((1u << shift) - 1u);
        let halfway = 1u << (shift - 1u);
        var r = half;
        if (rem > halfway || (rem == halfway && (half & 1u) == 1u)) {
            r = half + 1u;
        }
        return sign | r;
    }
    var h = (u32(e) << 10u) | (man >> 13u);
    let rem = man & 0x1fffu;
    if (rem > 0x1000u || (rem == 0x1000u && (h & 1u) == 1u)) {
        h = h + 1u;
    }
    return sign | h;
}

fn s3dp_f16x2(a: f32, b: f32) -> u32 {
    return s3dp_f16(a) | (s3dp_f16(b) << 16u);
}

// `cpu::plane_basis`.
fn s3dp_plane_basis(n: vec3<f32>) -> mat2x3<f32> {
    let helper = select(vec3<f32>(0.0, 1.0, 0.0), vec3<f32>(1.0, 0.0, 0.0), abs(n.y) > 0.99);
    let bx = s3dp_normalize_or(cross(helper, n), vec3<f32>(1.0, 0.0, 0.0));
    return mat2x3<f32>(bx, cross(n, bx));
}

@compute @workgroup_size(64)
fn scatter(
    @builtin(workgroup_id) wid: vec3<u32>,
    @builtin(num_workgroups) groups: vec3<u32>,
    @builtin(local_invocation_index) lane: u32,
) {
    let r = wid.x + wid.y * groups.x;
    if (r >= frame.b.x) {
        return;
    }
    if (lane == 0u) {
        atomicStore(&s3dp_prefix, 0u);
    }
    workgroupBarrier();
    let rec = s3dp_record(r);
    let j = rec.x & 0xffffffu;
    let job = s3dp_job(j, 0u);
    let group = s3dp_group(job.w);
    // The record's offset in its group: the totals of the blocks before its block, and the
    // counts of the records before it in its block.
    let within = r - group.x;
    let block = within / 64u;
    let before = within % 64u;
    var part = 0u;
    for (var b = lane; b < block; b = b + 64u) {
        part += atomicLoad(&work[frame.b.w + group.w + b]);
    }
    if (lane < before) {
        part += atomicLoad(&work[frame.b.z + group.x + block * 64u + lane]);
    }
    if (part != 0u) {
        atomicAdd(&s3dp_prefix, part);
    }
    workgroupBarrier();
    let lo = atomicLoad(&work[2u * r]);
    let hi = atomicLoad(&work[2u * r + 1u]);
    let bit = lane & 31u;
    let word = select(hi, lo, lane < 32u);
    if (((word >> bit) & 1u) == 0u) {
        return;
    }
    let below = countOneBits(word & ((1u << bit) - 1u)) + select(countOneBits(lo), 0u, lane < 32u);
    let out = group.z + atomicLoad(&s3dp_prefix) + below;
    let p = pool[s3dp_slot(rec, lane)];
    let e = job.x;
    let flags = s3dp_word(e, 1u).x;
    let local = (flags & S3DP_LOCAL) != 0u;
    let tint_in = bitcast<vec4<f32>>(s3dp_job(j, 1u));
    let place = bitcast<vec4<f32>>(s3dp_job(j, 2u));
    let pose_rot = bitcast<vec4<f32>>(s3dp_job(j, 3u));
    let scale = place.w;

    // `cpu::pack_entry`.
    let dt = frame.c.x;
    let life = p.vel_life.w;
    let age = p.pos_age.w + dt;
    let t = age / life;
    let seed = p.misc.y;
    var moving = dt;
    if (p.pos_age.w < 0.0) {
        moving = max(age, 0.0);
    }
    let v = p.vel_life.xyz;
    var pos = p.pos_age.xyz + v * moving;
    var vel = v;
    if (local) {
        pos = place.xyz + s3dp_rotate(pose_rot, pos);
        vel = s3dp_rotate(pose_rot, vel);
    }
    let w17 = s3dp_wordf(e, 17u);
    let w18 = s3dp_wordf(e, 18u);
    var size = s3dp_range(seed, S3DP_SIZE, w17.x, w17.y) * scale;
    if ((flags & S3DP_MIRRORED) != 0u && s3dp_rnd(seed, S3DP_MIRROR) < 0.5) {
        size = -size;
    }
    var rot = s3dp_range(seed, S3DP_ROTATION, w17.z, w17.w) + s3dp_range(seed, S3DP_ANGULAR, w18.x, w18.y) * age;
    var dir = vel;
    if ((flags & S3DP_PLANE) != 0u) {
        dir = s3dp_wordf(e, 22u).xyz;
        if (local) {
            dir = s3dp_rotate(pose_rot, dir);
        }
    } else if ((flags & S3DP_EMITTER_PLANE) != 0u) {
        var x = s3dp_oct_decode(p.misc.x);
        var n = s3dp_oct_decode(p.misc.w);
        if (local) {
            x = s3dp_rotate(pose_rot, x);
            n = s3dp_rotate(pose_rot, n);
        }
        let basis = s3dp_plane_basis(n);
        rot += atan2(dot(x, basis[1]), dot(x, basis[0]));
        dir = n;
    }
    let ca = s3dp_wordf(e, 19u);
    let cb = s3dp_wordf(e, 20u);
    let start = ca + (cb - ca) * s3dp_rnd(seed, S3DP_COLOR);
    var tint = vec4<f32>(1.0);
    if ((flags & S3DP_TINTED) != 0u) {
        tint = tint_in;
    }
    var rgb = tint.rgb * start.rgb;
    if ((flags & S3DP_FLICKERS) != 0u) {
        let fl = s3dp_wordf(e, 21u);
        let phase = s3dp_rnd(seed, S3DP_FLICKER);
        let wave = 0.5 + 0.5 * sin(S3DP_TAU * (fl.x * age + phase));
        rgb *= 1.0 - clamp(fl.y, 0.0, 1.0) * wave;
    }
    let alpha = tint.w * start.w;
    var a = alpha;
    if ((flags & S3DP_PREMULTIPLIED) != 0u) {
        rgb *= alpha;
        a = s3dp_range(seed, S3DP_COVERAGE, w18.z, w18.w) * alpha;
    }
    rot -= S3DP_TAU * round(rot / S3DP_TAU);
    let o = out * 10u;
    instances[o] = bitcast<u32>(pos.x);
    instances[o + 1u] = bitcast<u32>(pos.y);
    instances[o + 2u] = bitcast<u32>(pos.z);
    instances[o + 3u] = bitcast<u32>(size);
    instances[o + 4u] = s3dp_f16x2(dir.x, dir.y);
    instances[o + 5u] = s3dp_f16x2(dir.z, t);
    instances[o + 6u] = s3dp_f16x2(rgb.r, rgb.g);
    instances[o + 7u] = s3dp_f16x2(rgb.b, a);
    instances[o + 8u] = bitcast<u32>(rot);
    instances[o + 9u] = s3dp_pcg(seed + S3DP_BITS * S3DP_GOLDEN);
}
