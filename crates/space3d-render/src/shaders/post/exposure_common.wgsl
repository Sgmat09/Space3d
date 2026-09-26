// Space3d auto exposure: what the compute path (exposure.wgsl) and the render-pass path
// (exposure_dl.wgsl) share (graphics spec §5.2 PO2, design-post §5.2). The renderer prepends
// OUTPUT_WGSL (and FULLSCREEN_WGSL for the render-pass path). CPU mirrors: post/exposure.rs.

// Matches `GpuExposure` (post/exposure.rs): one aligned uniform slot per metered post group.
struct ExposureParams {
    // The post group's rect in S, clipped to S (x, y, w, h pixels).
    rect: vec4<u32>,
    // x: sample every `stride`-th pixel of each axis (1, 2 or 4); y: metering (0 average, 1
    // center weighted); z, w: -.
    sampling: vec4<u32>,
    // log2 luminance of the first bin, 1 / (log2 range), low and high percentile.
    range: vec4<f32>,
    // Metered exposure clamp: min EV, max EV; -, -.
    adapt: vec4<f32>,
    // Fraction of the gap to the target closed this frame when the scene got brighter (the
    // exposure goes down) and darker; 1 = snap (no history: first frame, cuts, re-enabled); -.
    step: vec4<f32>,
}

// Middle grey: the metered (percentile-filtered, log-average) luminance is exposed to it.
const S3D_EXPOSURE_KEY: f32 = 0.18;

// Metering weight of the rect pixel `p` (pixel center, relative to the rect's origin): 1 for
// `Average`; for `CenterWeighted` 4 at the center falling linearly to 1 at the corners.
fn s3d_exposure_weight(p: vec2<f32>) -> f32 {
    if (exposure.sampling.y == 0u) {
        return 1.0;
    }
    let d = (p / vec2<f32>(exposure.rect.zw) - 0.5) * 2.0;
    let r = clamp(length(d) * 0.70710678, 0.0, 1.0);
    return mix(4.0, 1.0, r);
}

// Luminance of a scene color (negative channels count as 0; NaN is not above anything).
fn s3d_exposure_luma(c: vec3<f32>) -> f32 {
    return s3d_luma(max(c, vec3<f32>(0.0)));
}

// Whether a luminance is metered: black (below the first bin) says nothing about exposure, so
// it is excluded (a space scene is mostly black).
fn s3d_exposure_metered(lum: f32) -> bool {
    return lum > exp2(exposure.range.x);
}
