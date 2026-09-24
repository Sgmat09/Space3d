// GPU frustum culling + stream compaction.
//
// One dispatch covers every culling job that reads the same source instance buffer for one
// view. Each workgroup finds its job with a binary search over `wg_start`, tests 64 instances
// against the frustum, compacts the visible ones inside the workgroup (one global atomic per
// workgroup instead of one per instance) and copies their raw instance words into the view's
// output region. `instance_count` of the job's DrawIndexedIndirectArgs accumulates the result.

struct CullParams {
    planes: array<vec4<f32>, 6>,
    job_start: u32,
    job_count: u32,
    total_workgroups: u32,
    workgroups_x: u32,
}

struct Job {
    sphere: vec4<f32>,      // mesh-local bounding sphere
    src_word: u32,          // first word of instance 0 in `src`
    count: u32,             // instances
    stride_words: u32,
    kind: u32,              // 0: model matrix, 1: translation (+ optional scale)
    t_off: u32,             // matrix / translation offset in words
    s_off: u32,             // scale offset in words
    s_kind: u32,            // 0: none, 1: uniform f32, 2: vec3
    dst_word: u32,          // first word of the output region
    draw: u32,              // index into indirect args
    wg_start: u32,          // first workgroup of this job within the dispatch
    pad0: u32,
    pad1: u32,
}

@group(0) @binding(0) var<uniform> params: CullParams;
@group(0) @binding(1) var<storage, read> jobs: array<Job>;
@group(0) @binding(2) var<storage, read> src: array<u32>;
@group(0) @binding(3) var<storage, read_write> dst: array<u32>;
@group(0) @binding(4) var<storage, read_write> args: array<atomic<u32>>;

var<workgroup> wg_count: atomic<u32>;
var<workgroup> wg_base: u32;

fn load3(o: u32) -> vec3<f32> {
    return vec3<f32>(bitcast<f32>(src[o]), bitcast<f32>(src[o + 1u]), bitcast<f32>(src[o + 2u]));
}

@compute @workgroup_size(64)
fn cull(@builtin(workgroup_id) wid: vec3<u32>, @builtin(local_invocation_index) lid: u32) {
    let wg = wid.x + wid.y * params.workgroups_x;
    if (wg >= params.total_workgroups) {
        return;
    }
    // Last job with wg_start <= wg.
    var lo = params.job_start;
    var hi = params.job_start + params.job_count - 1u;
    loop {
        if (lo >= hi) {
            break;
        }
        let mid = (lo + hi + 1u) / 2u;
        if (jobs[mid].wg_start <= wg) {
            lo = mid;
        } else {
            hi = mid - 1u;
        }
    }
    let job = jobs[lo];
    let local = (wg - job.wg_start) * 64u + lid;

    var visible = false;
    var base = 0u;
    if (local < job.count) {
        base = job.src_word + local * job.stride_words;
        var center: vec3<f32>;
        var radius: f32;
        if (job.kind == 0u) {
            let o = base + job.t_off;
            let c0 = load3(o);
            let c1 = load3(o + 4u);
            let c2 = load3(o + 8u);
            let t = load3(o + 12u);
            let s = sqrt(max(dot(c0, c0), max(dot(c1, c1), dot(c2, c2))));
            center = t + c0 * job.sphere.x + c1 * job.sphere.y + c2 * job.sphere.z;
            radius = job.sphere.w * s;
        } else {
            var s = 1.0;
            if (job.s_kind == 1u) {
                s = abs(bitcast<f32>(src[base + job.s_off]));
            } else if (job.s_kind == 2u) {
                let sv = abs(load3(base + job.s_off));
                s = max(sv.x, max(sv.y, sv.z));
            }
            // Rotation is not decoded here: grow the sphere to cover any orientation.
            center = load3(base + job.t_off);
            radius = (length(job.sphere.xyz) + job.sphere.w) * s;
        }
        visible = true;
        for (var i = 0u; i < 6u; i++) {
            let p = params.planes[i];
            if (dot(p.xyz, center) + p.w < -radius) {
                visible = false;
            }
        }
    }

    if (lid == 0u) {
        atomicStore(&wg_count, 0u);
    }
    workgroupBarrier();
    var slot = 0u;
    if (visible) {
        slot = atomicAdd(&wg_count, 1u);
    }
    workgroupBarrier();
    if (lid == 0u) {
        let n = atomicLoad(&wg_count);
        var b = 0u;
        if (n > 0u) {
            b = atomicAdd(&args[job.draw * 5u + 1u], n);
        }
        wg_base = b;
    }
    workgroupBarrier();
    if (visible) {
        let out = job.dst_word + (wg_base + slot) * job.stride_words;
        for (var w = 0u; w < job.stride_words; w++) {
            dst[out + w] = src[base + w];
        }
    }
}
