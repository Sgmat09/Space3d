// Full-screen triangle blit used for GPU mipmap generation (linear filtering; sRGB views
// decode on sample and encode on store, so downsampling happens in linear space).
//
// Vertices 3k..3k+2 draw the triangle for source layer k: array sources are sampled through a
// `D2Array` view of all layers (GL cannot sample a single layer of an array texture through a
// `D2` view), and the layer travels in the vertex index, so no per-layer uniforms are needed.
// The SOURCE marker is replaced with the source binding and `fs_main` (see `pipeline.rs`).

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) layer: i32,
}

@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> VsOut {
    let v = i % 3u;
    let uv = vec2<f32>(f32((v << 1u) & 2u), f32(v & 2u));
    var out: VsOut;
    out.pos = vec4<f32>(uv * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0), 0.0, 1.0);
    out.uv = uv;
    out.layer = i32(i / 3u);
    return out;
}

//#SOURCE
