// Space3d particles: the per-view back-to-front sort of `SortMode::ByDistance` layers on the GPU
// backend (design-particles §7 step 7; graphics spec §5.2 PA4). One sort job per (camera view,
// sorted draw group): the group's compacted instances (their count is in the group's
// `DrawIndirectArgs`) are ordered by view depth, farthest first, ties by instance index, into a
// region of the sorted instance buffer drawn with the same indirect arguments.
//
// A bitonic network over `n` = the group's capacity rounded up to a power of two (≤ 65 536):
// keys `(depth key, index)` in `keys[base..base + n]`, slots past the count hold the sentinel
// `(0xffffffff, index)` and sort last. Blocks of 512 keys sort in workgroup memory:
// * `sort_local` with `k == 0`: load (computing the keys), sort every block completely
//   (k = 2 … min(n, 512)), then store the keys, or gather the instances when `n ≤ 512`;
// * `sort_global`: one compare-exchange step (k, j) with j ≥ 512 over all of `keys`;
// * `sort_local` with `k > 512`: the steps j = 256 … 1 of stage k within each block, then store
//   the keys, or gather the instances after the last stage (`k == n`).
// That is 1 dispatch up to 512, and 1 + Σ over the stages k = 1024 … n of (log2 k − 8): 3 at
// 1024, 6 at 2048, 21 at 16 384, 36 at 65 536. Keys are unique (the index), so the order is
// deterministic whatever the order of execution. At most 4 storage buffers, `@workgroup_size(64)`.

struct SortParams {
    // x: first key of the job, y: n (a power of two), z: word of the instance count (the group's
    // `DrawIndirectArgs::instance_count`), w: first instance of the group.
    a: vec4<u32>,
    // x: first instance of the job's sorted region, y: stage k (0: the initial local sort), z:
    // step j (`sort_global`), w: 1 = the last pass (gather the sorted instances).
    b: vec4<u32>,
    // View-space z of a world position p: dot(row.xyz, p) + row.w (the camera looks down −z:
    // farther is more negative).
    row: vec4<f32>,
}

@group(0) @binding(0) var<storage, read> instances: array<u32>;
@group(0) @binding(1) var<storage, read> work: array<u32>;
@group(0) @binding(2) var<storage, read_write> keys: array<vec2<u32>>;
@group(0) @binding(3) var<storage, read_write> sorted: array<u32>;
@group(0) @binding(4) var<uniform> params: SortParams;

// Words per instance (`ParticleInstance`, 40 bytes).
const INSTANCE_WORDS: u32 = 10u;
const BLOCK: u32 = 512u;
const SENTINEL: u32 = 0xffffffffu;

var<workgroup> block: array<vec2<u32>, 512>;

// An order-preserving u32 of a float (ascending u32 = ascending float).
fn order_key(f: f32) -> u32 {
    let bits = bitcast<u32>(f);
    return select(bits | 0x80000000u, ~bits, (bits & 0x80000000u) != 0u);
}

// (key, index) < (key, index), lexicographically.
fn less(a: vec2<u32>, b: vec2<u32>) -> bool {
    return a.x < b.x || (a.x == b.x && a.y < b.y);
}

// The key of element `i` of the job: its instance's view depth, or the sentinel past the count.
fn load_key(i: u32, count: u32) -> vec2<u32> {
    if (i >= count) {
        return vec2<u32>(SENTINEL, i);
    }
    let w = (params.a.w + i) * INSTANCE_WORDS;
    let p = vec3<f32>(bitcast<f32>(instances[w]), bitcast<f32>(instances[w + 1u]), bitcast<f32>(instances[w + 2u]));
    let z = dot(params.row.xyz, p) + params.row.w;
    return vec2<u32>(order_key(z), i);
}

// One compare-exchange step (k, j) within the workgroup's block (local indices, the direction
// from the global index `first + i`).
fn local_step(lid: u32, first: u32, k: u32, j: u32, n: u32) {
    for (var q = 0u; q < BLOCK / 128u; q = q + 1u) {
        let t = lid + q * 64u;
        let i = 2u * j * (t / j) + t % j;
        let l = i + j;
        if (first + l < n) {
            let up = ((first + i) & k) == 0u;
            let x = block[i];
            let y = block[l];
            if (less(y, x) == up) {
                block[i] = y;
                block[l] = x;
            }
        }
    }
    workgroupBarrier();
}

// Store the block's keys, or gather the sorted instances (the last pass).
fn store(lid: u32, first: u32, n: u32, count: u32) {
    for (var q = 0u; q < BLOCK / 64u; q = q + 1u) {
        let i = lid + q * 64u;
        let g = first + i;
        if (g >= n) {
            continue;
        }
        let kv = block[i];
        if (params.b.w == 0u) {
            keys[params.a.x + g] = kv;
        } else if (g < count) {
            let src = (params.a.w + kv.y) * INSTANCE_WORDS;
            let dst = (params.b.x + g) * INSTANCE_WORDS;
            for (var w = 0u; w < INSTANCE_WORDS; w = w + 1u) {
                sorted[dst + w] = instances[src + w];
            }
        }
    }
}

@compute @workgroup_size(64)
fn sort_local(@builtin(workgroup_id) wg: vec3<u32>, @builtin(local_invocation_index) lid: u32) {
    let n = params.a.y;
    let count = min(work[params.a.z], n);
    let first = wg.x * BLOCK;
    let stage = params.b.y;
    for (var q = 0u; q < BLOCK / 64u; q = q + 1u) {
        let i = lid + q * 64u;
        let g = first + i;
        if (g < n) {
            if (stage == 0u) {
                block[i] = load_key(g, count);
            } else {
                block[i] = keys[params.a.x + g];
            }
        }
    }
    workgroupBarrier();
    if (stage == 0u) {
        let top = min(n, BLOCK);
        for (var k = 2u; k <= top; k = k * 2u) {
            for (var j = k / 2u; j > 0u; j = j / 2u) {
                local_step(lid, first, k, j, n);
            }
        }
    } else {
        for (var j = BLOCK / 2u; j > 0u; j = j / 2u) {
            local_step(lid, first, stage, j, n);
        }
    }
    store(lid, first, n, count);
}

@compute @workgroup_size(64)
fn sort_global(@builtin(global_invocation_id) gid: vec3<u32>) {
    // n / 2 ≤ 32 768 pairs: at most 512 workgroups along x.
    let t = gid.x;
    let n = params.a.y;
    if (t >= n / 2u) {
        return;
    }
    let k = params.b.y;
    let j = params.b.z;
    let i = 2u * j * (t / j) + t % j;
    let l = i + j;
    let up = (i & k) == 0u;
    let x = keys[params.a.x + i];
    let y = keys[params.a.x + l];
    if (less(y, x) == up) {
        keys[params.a.x + i] = y;
        keys[params.a.x + l] = x;
    }
}
