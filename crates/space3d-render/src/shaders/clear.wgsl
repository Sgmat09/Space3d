// Viewport clear: one triangle covering the pass's viewport (the pass's scissor rect bounds it
// as well). Its color is the pass's blend constant — blend factors Constant / Zero write the
// constant as is — so no buffer or bind group is needed, and one pipeline per target format
// and sample count clears to any color. Depth is neither tested nor written.

@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let uv = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4<f32>(uv * 2.0 - 1.0, 0.0, 1.0);
}

@fragment
fn fs_main() -> @location(0) vec4<f32> {
    return vec4<f32>(1.0);
}
