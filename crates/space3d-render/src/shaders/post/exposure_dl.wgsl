// Space3d auto exposure, render-pass path (graphics spec §5.2 PO2, design-post §5.2): no compute
// (WebGL2, GLES 3.0, `DownlevelOverride::no_compute`). exposure_common.wgsl and FULLSCREEN_WGSL
// come first. Texel loads only (no sampling, no derivatives: WebGL2-safe). Per metered post group:
//
// * `fs_loglum`: the rect in S → a chain level a quarter of its size (`Rgba16Float`): each texel
//   holds the metering-weighted log2 luminance of the samples of its 4 × 4 block (one per
//   `stride` pixels per axis, the compute path's samples), R = Σ w·log2 L / 16 and G = Σ w / 16.
//   Black is not metered (weight 0); log2 L is clamped to the histogram's range.
// * `fs_reduce`: a level → one an eighth of its size, summing 8 × 8 texels (/ 64), while the
//   level is larger than 16 × 16.
// * `fs_adapt`: sums the last level, takes the weighted mean log2 luminance R / G, the target
//   exposure `log2(0.18) − mean` clamped to [min EV, max EV], and adapts the previous frame's E
//   toward it: writes this frame's 1×1 E (a ping-pong pair per camera).
//
// No percentile filter (it needs a histogram): the plain weighted mean; it matches the compute
// path within ±0.5 EV on typical scenes.

@group(0) @binding(0) var<uniform> exposure: ExposureParams;
@group(1) @binding(0) var src_tex: texture_2d<f32>;
@group(1) @binding(1) var prev_tex: texture_2d<f32>;

@fragment
fn fs_loglum(in: FullscreenOut) -> @location(0) vec4<f32> {
    let block = vec2<u32>(in.pos.xy) * 4u;
    let stride = exposure.sampling.x;
    let lo = exposure.range.x;
    let hi = exposure.range.x + 1.0 / exposure.range.y;
    var r = 0.0;
    var g = 0.0;
    for (var y = 0u; y < 4u; y += stride) {
        for (var x = 0u; x < 4u; x += stride) {
            let p = block + vec2<u32>(x, y);
            if (all(p < exposure.rect.zw)) {
                let c = textureLoad(src_tex, vec2<i32>(exposure.rect.xy + p), 0).rgb;
                let lum = s3d_exposure_luma(c);
                if (s3d_exposure_metered(lum)) {
                    let w = s3d_exposure_weight(vec2<f32>(p) + 0.5);
                    r += w * clamp(log2(lum), lo, hi);
                    g += w;
                }
            }
        }
    }
    return vec4<f32>(r / 16.0, g / 16.0, 0.0, 1.0);
}

@fragment
fn fs_reduce(in: FullscreenOut) -> @location(0) vec4<f32> {
    let size = textureDimensions(src_tex);
    let base = vec2<u32>(in.pos.xy) * 8u;
    var acc = vec2<f32>(0.0);
    for (var y = 0u; y < 8u; y += 1u) {
        for (var x = 0u; x < 8u; x += 1u) {
            let p = base + vec2<u32>(x, y);
            if (all(p < size)) {
                acc += textureLoad(src_tex, vec2<i32>(p), 0).rg;
            }
        }
    }
    return vec4<f32>(acc / 64.0, 0.0, 1.0);
}

@fragment
fn fs_adapt(in: FullscreenOut) -> @location(0) vec4<f32> {
    let size = textureDimensions(src_tex);
    var acc = vec2<f32>(0.0);
    for (var y = 0u; y < size.y; y += 1u) {
        for (var x = 0u; x < size.x; x += 1u) {
            acc += textureLoad(src_tex, vec2<i32>(vec2<u32>(x, y)), 0).rg;
        }
    }
    let snap = exposure.step.z > 0.5;
    let previous = select(textureLoad(prev_tex, vec2<i32>(0), 0).r, 0.0, snap);
    // Nothing metered (all black): hold the exposure (0 EV on a snap).
    var target_ev = previous;
    if (acc.y > 0.0) {
        target_ev = log2(S3D_EXPOSURE_KEY) - acc.x / acc.y;
    }
    target_ev = clamp(target_ev, exposure.adapt.x, exposure.adapt.y);
    var ev = target_ev;
    if (!snap) {
        let k = select(exposure.step.y, exposure.step.x, target_ev < previous);
        ev = previous + (target_ev - previous) * k;
    }
    return vec4<f32>(ev, 0.0, 0.0, 1.0);
}
