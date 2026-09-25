// Space3d deterministic hashes (bit-identical to the Rust mirrors in `output.rs`).

fn s3d_pcg(v: u32) -> u32 {
    let s = v * 747796405u + 2891336453u;
    let w = ((s >> ((s >> 28u) + 4u)) ^ s) * 277803737u;
    return (w >> 22u) ^ w;
}

// Uniform 0..1 per pixel `p` and `seed`.
fn s3d_hash01(p: vec2<u32>, seed: u32) -> f32 {
    return f32(s3d_pcg(p.x ^ s3d_pcg(p.y ^ s3d_pcg(seed)))) * (1.0 / 4294967295.0);
}
