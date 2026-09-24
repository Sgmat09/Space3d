// Draw compaction for `multi_draw_indexed_indirect_count`: copies every non-empty
// DrawIndexedIndirectArgs of a view into its run's slice of `args_out` and counts them, so
// batches that were fully culled cost nothing on the GPU command processor.

struct CompactParams {
    draw_start: u32,
    draw_count: u32,
    pad0: u32,
    pad1: u32,
}

@group(0) @binding(0) var<uniform> cparams: CompactParams;
@group(0) @binding(1) var<storage, read> draw_meta: array<vec2<u32>>; // (run index, run's first draw)
@group(0) @binding(2) var<storage, read> args_in: array<u32>;
@group(0) @binding(3) var<storage, read_write> counts: array<atomic<u32>>;
@group(0) @binding(4) var<storage, read_write> args_out: array<u32>;

@compute @workgroup_size(64)
fn compact(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= cparams.draw_count) {
        return;
    }
    let d = cparams.draw_start + gid.x;
    if (args_in[d * 5u + 1u] == 0u) {
        return;
    }
    let m = draw_meta[d];
    let slot = atomicAdd(&counts[m.x], 1u);
    let o = (m.y + slot) * 5u;
    for (var k = 0u; k < 5u; k++) {
        args_out[o + k] = args_in[d * 5u + k];
    }
}
