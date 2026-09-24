// Draw compaction for `multi_draw_indexed_indirect_count`: copies the non-empty
// DrawIndexedIndirectArgs of each multi-draw run to the front of the run's slice of
// `args_out`, in their original order, and stores how many there are. Batches that were fully
// culled then cost nothing on the GPU command processor, and draw order stays deterministic.
//
// One workgroup per run: each lane counts the non-empty draws of a contiguous block, a
// workgroup prefix sum gives every block its first output slot.

struct CompactParams {
    run_start: u32,
    run_count: u32,
    workgroups_x: u32,
    pad0: u32,
}

@group(0) @binding(0) var<uniform> cparams: CompactParams;
@group(0) @binding(1) var<storage, read> runs: array<vec2<u32>>; // (first draw, draw count)
@group(0) @binding(2) var<storage, read> args_in: array<u32>;
@group(0) @binding(3) var<storage, read_write> counts: array<u32>;
@group(0) @binding(4) var<storage, read_write> args_out: array<u32>;

var<workgroup> sums: array<u32, 64>;

fn non_empty(d: u32) -> u32 {
    return select(0u, 1u, args_in[d * 5u + 1u] != 0u);
}

@compute @workgroup_size(64)
fn compact(@builtin(workgroup_id) wid: vec3<u32>, @builtin(local_invocation_index) lid: u32) {
    let i = wid.x + wid.y * cparams.workgroups_x;
    if (i >= cparams.run_count) {
        return;
    }
    let r = cparams.run_start + i;
    let run = runs[r];
    let first = run.x;
    let n = run.y;
    let per = (n + 63u) / 64u;
    let begin = min(lid * per, n);
    let end = min(begin + per, n);
    var sum = 0u;
    for (var k = begin; k < end; k++) {
        sum += non_empty(first + k);
    }
    // Inclusive prefix sum over the 64 lanes (Hillis–Steele).
    sums[lid] = sum;
    workgroupBarrier();
    for (var off = 1u; off < 64u; off = off * 2u) {
        var x = sums[lid];
        if (lid >= off) {
            x += sums[lid - off];
        }
        workgroupBarrier();
        sums[lid] = x;
        workgroupBarrier();
    }
    var slot = sums[lid] - sum;
    for (var k = begin; k < end; k++) {
        let d = first + k;
        if (non_empty(d) == 1u) {
            let o = (first + slot) * 5u;
            for (var w = 0u; w < 5u; w++) {
                args_out[o + w] = args_in[d * 5u + w];
            }
            slot += 1u;
        }
    }
    if (lid == 63u) {
        counts[r] = sums[63];
    }
}
