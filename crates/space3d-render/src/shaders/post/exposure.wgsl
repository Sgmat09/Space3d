// Space3d auto exposure, compute path (graphics spec §5.2 PO2, design-post §5.2): one compute
// pass of two dispatches per metered post group (exposure_common.wgsl comes first).
//
// * `cs_histogram`: 16 × 16 workgroups over the group's rect in S, one sample per `stride`
//   pixels per axis. Each metered sample adds its weight (16 × the metering weight, rounded: an
//   integer) to one of 255 log2-luminance bins in workgroup memory, and its weight × its position
//   inside the bin (6 bits) to the bin's second counter, so a bin's samples are averaged at their
//   mean position, not the bin's center (no 1/8 EV steps); the workgroup then adds its
//   non-empty bins to the global histogram. Bin 0 is black (never metered).
// * `cs_average`: one workgroup of 256: an inclusive prefix sum of the bins, the part of every
//   bin inside the [low, high] percentile window, their weighted mean log2 luminance (each bin at
//   its samples' mean position), the target exposure `log2(0.18) − mean` clamped to [min EV, max EV], and the
//   adaptation toward it. It writes the camera's state (the renderer copies `ev` into the 1×1
//   exposure texture E) and clears the histogram for the next group.
//
// Integer atomics are order-independent and the float reductions run in a fixed order: the
// result is bit-for-bit deterministic.

struct ExposureState {
    // The adapted exposure (stops): what E holds.
    ev: f32,
    // This frame's target.
    target_ev: f32,
    // Metered mean log2 luminance (the last one when nothing was metered).
    avg_log2: f32,
    // 0 until the first metering (zero-initialized buffer): the first frame snaps.
    valid: u32,
}

@group(0) @binding(0) var<uniform> exposure: ExposureParams;
@group(0) @binding(1) var scene_tex: texture_2d<f32>;
// Bins 0..256: weights; 256..512: weight × position inside the bin (0..63).
@group(0) @binding(2) var<storage, read_write> histogram: array<atomic<u32>, 512>;
@group(0) @binding(3) var<storage, read_write> state: ExposureState;

var<workgroup> local_bins: array<atomic<u32>, 512>;
var<workgroup> scan: array<u32, 256>;
var<workgroup> sum_w: array<f32, 256>;
var<workgroup> sum_wl: array<f32, 256>;

// The bin of a metered luminance, 1 + ⌊255 · x⌋ over x = (log2 lum − first) / range in 0..1
// (clamped: bins 1 and 255 take everything beyond the range), and its position inside the bin
// in 64ths.
fn s3d_exposure_bin(lum: f32) -> vec2<u32> {
    let x = clamp((log2(lum) - exposure.range.x) * exposure.range.y, 0.0, 1.0) * 255.0;
    let bin = min(u32(x), 254u);
    return vec2<u32>(1u + bin, min(u32((x - f32(bin)) * 64.0), 63u));
}

@compute @workgroup_size(16, 16)
fn cs_histogram(@builtin(global_invocation_id) g: vec3<u32>, @builtin(local_invocation_index) li: u32) {
    atomicStore(&local_bins[li], 0u);
    atomicStore(&local_bins[li + 256u], 0u);
    workgroupBarrier();
    let p = g.xy * exposure.sampling.x;
    if (all(p < exposure.rect.zw)) {
        let c = textureLoad(scene_tex, vec2<i32>(exposure.rect.xy + p), 0).rgb;
        let lum = s3d_exposure_luma(c);
        if (s3d_exposure_metered(lum)) {
            let w = u32(s3d_exposure_weight(vec2<f32>(p) + 0.5) * 16.0 + 0.5);
            let bin = s3d_exposure_bin(lum);
            atomicAdd(&local_bins[bin.x], w);
            atomicAdd(&local_bins[bin.x + 256u], w * bin.y);
        }
    }
    workgroupBarrier();
    let n = atomicLoad(&local_bins[li]);
    if (n > 0u) {
        atomicAdd(&histogram[li], n);
        atomicAdd(&histogram[li + 256u], atomicLoad(&local_bins[li + 256u]));
    }
}

@compute @workgroup_size(256)
fn cs_average(@builtin(local_invocation_index) li: u32) {
    let count = atomicLoad(&histogram[li]);
    let positions = atomicLoad(&histogram[li + 256u]);
    // Cleared for the next metered group (this frame's next one, or the next frame).
    atomicStore(&histogram[li], 0u);
    atomicStore(&histogram[li + 256u], 0u);
    scan[li] = count;
    workgroupBarrier();
    // Hillis–Steele inclusive prefix sum (fixed order).
    for (var offset = 1u; offset < 256u; offset = offset * 2u) {
        var v = scan[li];
        if (li >= offset) {
            v = v + scan[li - offset];
        }
        workgroupBarrier();
        scan[li] = v;
        workgroupBarrier();
    }
    let incl = f32(scan[li]);
    let excl = f32(scan[li] - count);
    let total = f32(scan[255]);
    let lo = total * exposure.range.z;
    let hi = total * exposure.range.w;
    // The part of this bin's weight inside the percentile window.
    let portion = max(min(incl, hi) - max(excl, lo), 0.0);
    // The bin's samples' mean position (bin 0, black, never counts).
    let inside = (f32(positions) / f32(max(count, 1u)) + 0.5) / 64.0;
    let center = exposure.range.x + (f32(li) - 1.0 + inside) / (255.0 * exposure.range.y);
    sum_w[li] = select(portion, 0.0, li == 0u);
    sum_wl[li] = select(portion * center, 0.0, li == 0u);
    workgroupBarrier();
    for (var s = 128u; s > 0u; s = s >> 1u) {
        if (li < s) {
            sum_w[li] = sum_w[li] + sum_w[li + s];
            sum_wl[li] = sum_wl[li] + sum_wl[li + s];
        }
        workgroupBarrier();
    }
    if (li == 0u) {
        let snap = exposure.step.z > 0.5 || state.valid == 0u;
        let previous = select(state.ev, 0.0, snap);
        var avg = select(state.avg_log2, log2(S3D_EXPOSURE_KEY), snap);
        // Nothing metered (all black): hold the exposure (0 EV on a snap).
        var target_ev = previous;
        if (sum_w[0] > 0.0) {
            avg = sum_wl[0] / sum_w[0];
            target_ev = log2(S3D_EXPOSURE_KEY) - avg;
        }
        target_ev = clamp(target_ev, exposure.adapt.x, exposure.adapt.y);
        var ev = target_ev;
        if (!snap) {
            let k = select(exposure.step.y, exposure.step.x, target_ev < previous);
            ev = previous + (target_ev - previous) * k;
        }
        state.ev = ev;
        state.target_ev = target_ev;
        state.avg_log2 = avg;
        state.valid = 1u;
    }
}
