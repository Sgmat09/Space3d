//! Reader for baked hull envelopes (`.sbe`), produced with the SpaceCorps models outside this
//! repository. A missing or invalid file is not an error for the demo: the envelope is then
//! computed at load time (`envelope.rs`).
//!
//! Layout (little endian): "SCSE", u32 version (1), u32 vertex count, u32 index count, u32 flags,
//! f32 offset (fraction of the model length), f32 × 3 bounds min, f32 × 3 bounds max, then the
//! positions (u16 × 3, quantised in the bounds), the normals (octahedral, i8 × 2) and the indices
//! (u16 when there are at most 65 535 vertices, else u32).

pub struct Sbe {
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub indices: Vec<u32>,
}

pub fn decode(b: &[u8]) -> Result<Sbe, String> {
    let u32_at = |o: usize| b.get(o..o + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]));
    let f32_at = |o: usize| u32_at(o).map(f32::from_bits);
    if b.len() < 48 || &b[0..4] != b"SCSE" || u32_at(4) != Some(1) {
        return Err("not a version 1 .sbe file".into());
    }
    let nv = u32_at(8).unwrap_or(0) as usize;
    let ni = u32_at(12).unwrap_or(0) as usize;
    let lo = [f32_at(24), f32_at(28), f32_at(32)].map(|v| v.unwrap_or(0.0));
    let hi = [f32_at(36), f32_at(40), f32_at(44)].map(|v| v.unwrap_or(0.0));
    let small = nv <= 65_535;
    // in u64: u32 counts cannot overflow it, even on 32-bit targets (wasm32)
    let need = 48u64 + nv as u64 * 8 + ni as u64 * if small { 2 } else { 4 };
    if (b.len() as u64) < need {
        return Err(format!(".sbe truncated: {} bytes, {need} expected", b.len()));
    }
    let mut o = 48;
    let mut positions = Vec::with_capacity(nv);
    for _ in 0..nv {
        let mut p = [0.0; 3];
        for (k, v) in p.iter_mut().enumerate() {
            let q = u16::from_le_bytes([b[o], b[o + 1]]) as f32 / 65_535.0;
            *v = lo[k] + q * (hi[k] - lo[k]);
            o += 2;
        }
        positions.push(p);
    }
    let mut normals = Vec::with_capacity(nv);
    for _ in 0..nv {
        let mut x = (b[o] as i8) as f32 / 127.0;
        let mut y = (b[o + 1] as i8) as f32 / 127.0;
        o += 2;
        let z = 1.0 - x.abs() - y.abs();
        if z < 0.0 {
            let ox = x;
            x = (1.0 - y.abs()) * if ox >= 0.0 { 1.0 } else { -1.0 };
            y = (1.0 - ox.abs()) * if y >= 0.0 { 1.0 } else { -1.0 };
        }
        let l = (x * x + y * y + z * z).sqrt().max(1e-6);
        normals.push([x / l, y / l, z / l]);
    }
    let mut indices = Vec::with_capacity(ni);
    for _ in 0..ni {
        if small {
            indices.push(u16::from_le_bytes([b[o], b[o + 1]]) as u32);
            o += 2;
        } else {
            indices.push(u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]));
            o += 4;
        }
    }
    if indices.iter().any(|&i| i as usize >= nv) {
        return Err(".sbe index out of range".into());
    }
    Ok(Sbe {
        positions,
        normals,
        indices,
    })
}
