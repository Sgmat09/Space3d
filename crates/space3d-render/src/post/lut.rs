//! 3D LUTs (graphics spec §5.2 PO4; design-post §3.6): the Adobe / Resolve `.cube` parser behind
//! [`super::api::ColorLut::from_cube`], and the CPU mirror of the composite's strip lookup.
//!
//! A LUT is an N²×N strip texture: slice `b` (blue) at x ∈ [b·N, (b + 1)·N), red along x inside
//! the slice, green along y. The composite indexes it by the sRGB-encoded display color and
//! blends 2 bilinear taps between the neighbouring blue slices.

use super::api::LutError;
use crate::assets::{AddressMode, FilterMode, SamplerSettings, TextureSource};
use crate::instance::f32_to_f16;

/// Largest `LUT_3D_SIZE` accepted (a 64³ strip is 4096 × 64).
pub(crate) const MAX_SIZE: u32 = 64;

/// Parse a `.cube` 3D LUT into an `Rgba16Float` strip over the domain 0..1 (a LUT with another
/// `DOMAIN_MIN`/`DOMAIN_MAX` is resampled onto it, trilinearly).
pub(crate) fn parse_cube(text: &str) -> Result<TextureSource, LutError> {
    let mut size: Option<u32> = None;
    let mut min = [0.0f32; 3];
    let mut max = [1.0f32; 3];
    let mut values: Vec<[f32; 3]> = Vec::new();
    let parse_floats = |words: &mut std::str::SplitWhitespace<'_>, line: usize, n: usize| {
        let v: Vec<f32> = words
            .map(|w| {
                w.parse::<f32>().map_err(|_| LutError::Parse {
                    line,
                    message: format!("`{w}` is not a number"),
                })
            })
            .collect::<Result<_, _>>()?;
        if v.len() != n || v.iter().any(|x| !x.is_finite()) {
            return Err(LutError::Parse {
                line,
                message: format!("expected {n} finite numbers"),
            });
        }
        Ok(v)
    };
    for (i, raw) in text.lines().enumerate() {
        let line = i + 1;
        let content = raw.split('#').next().unwrap_or("").trim();
        if content.is_empty() {
            continue;
        }
        let mut words = content.split_whitespace();
        let Some(first) = words.next() else { continue };
        match first {
            // A quoted title may contain anything.
            "TITLE" => {}
            "LUT_3D_SIZE" => {
                let v = words
                    .next()
                    .and_then(|w| w.parse::<u32>().ok())
                    .ok_or(LutError::Parse {
                        line,
                        message: "LUT_3D_SIZE needs a whole number".into(),
                    })?;
                if !(2..=MAX_SIZE).contains(&v) {
                    return Err(LutError::Size(v));
                }
                size = Some(v);
                values.reserve((v * v * v) as usize);
            }
            "LUT_1D_SIZE" | "LUT_1D_INPUT_RANGE" => {
                return Err(LutError::Parse {
                    line,
                    message: "1D LUTs are not supported (a 3D LUT is needed)".into(),
                });
            }
            "DOMAIN_MIN" => min.copy_from_slice(&parse_floats(&mut words, line, 3)?),
            "DOMAIN_MAX" => max.copy_from_slice(&parse_floats(&mut words, line, 3)?),
            "LUT_3D_INPUT_RANGE" => {
                let v = parse_floats(&mut words, line, 2)?;
                min = [v[0]; 3];
                max = [v[1]; 3];
            }
            w if w.starts_with(|c: char| c.is_ascii_digit() || matches!(c, '-' | '+' | '.')) => {
                let mut all = content.split_whitespace();
                let v = parse_floats(&mut all, line, 3)?;
                values.push([v[0], v[1], v[2]]);
            }
            // Other keywords (vendor extensions) are ignored.
            _ => {}
        }
    }
    let n = size.ok_or(LutError::MissingSize)?;
    let expected = (n * n * n) as usize;
    if values.len() != expected {
        return Err(LutError::Count {
            expected,
            found: values.len(),
        });
    }
    if (0..3).any(|c| max[c] <= min[c]) {
        return Err(LutError::Parse {
            line: 0,
            message: format!("DOMAIN_MAX {max:?} must exceed DOMAIN_MIN {min:?}"),
        });
    }
    let at = |r: usize, g: usize, b: usize| values[r + g * n as usize + b * (n * n) as usize];
    let identity_domain = min == [0.0; 3] && max == [1.0; 3];
    let last = (n - 1) as f32;
    // The value for the input `t` (0..1 per channel): the table entry, or trilinear inside the
    // table when the domain is not 0..1 (inputs outside it clamp to its edge).
    let sample = |t: [f32; 3]| -> [f32; 3] {
        let u: [f32; 3] = std::array::from_fn(|c| ((t[c] - min[c]) / (max[c] - min[c]) * last).clamp(0.0, last));
        let i0: [usize; 3] = std::array::from_fn(|c| u[c].floor() as usize);
        let i1: [usize; 3] = std::array::from_fn(|c| (i0[c] + 1).min(n as usize - 1));
        let f: [f32; 3] = std::array::from_fn(|c| u[c] - i0[c] as f32);
        let mut out = [0.0f32; 3];
        for corner in 0..8 {
            let pick = |c: usize| corner >> c & 1 == 1;
            let w: f32 = (0..3).map(|c| if pick(c) { f[c] } else { 1.0 - f[c] }).product();
            if w == 0.0 {
                continue;
            }
            let idx: [usize; 3] = std::array::from_fn(|c| if pick(c) { i1[c] } else { i0[c] });
            let v = at(idx[0], idx[1], idx[2]);
            for c in 0..3 {
                out[c] += w * v[c];
            }
        }
        out
    };
    let (w, h) = (n * n, n);
    let mut data = Vec::with_capacity((w * h * 8) as usize);
    for y in 0..h as usize {
        for x in 0..w as usize {
            let (r, g, b) = (x % n as usize, y, x / n as usize);
            let v = if identity_domain {
                at(r, g, b)
            } else {
                sample([r as f32 / last, g as f32 / last, b as f32 / last])
            };
            for c in [v[0], v[1], v[2], 1.0] {
                data.extend_from_slice(&f32_to_f16(c).to_le_bytes());
            }
        }
    }
    Ok(TextureSource {
        width: w,
        height: h,
        layers: 1,
        format: wgpu::TextureFormat::Rgba16Float,
        mip_level_count: 1,
        data,
        generate_mipmaps: false,
        sampler: SamplerSettings {
            filter: FilterMode::Linear,
            address: AddressMode::Clamp,
        },
    })
}

/// CPU mirror of the composite's `post_lut` (tests): the strip lookup of the sRGB-encoded color
/// `e` (0..1) in a strip of `n` whose texel `(x, y)` is `texel(x, y)`, with bilinear taps.
#[cfg(test)]
pub(crate) fn lookup(e: [f32; 3], n: u32, texel: impl Fn(u32, u32) -> [f32; 3]) -> [f32; 3] {
    let nf = n as f32;
    let bilinear = |u: f32, v: f32| {
        let (w, h) = ((n * n) as f32, nf);
        let (px, py) = ((u * w - 0.5).clamp(0.0, w - 1.0), (v * h - 0.5).clamp(0.0, h - 1.0));
        let (x0, y0) = (px.floor() as u32, py.floor() as u32);
        let (x1, y1) = ((x0 + 1).min(n * n - 1), (y0 + 1).min(n - 1));
        let (fx, fy) = (px - x0 as f32, py - y0 as f32);
        let mix = |a: [f32; 3], b: [f32; 3], t: f32| std::array::from_fn::<f32, 3, _>(|c| a[c] + (b[c] - a[c]) * t);
        mix(
            mix(texel(x0, y0), texel(x1, y0), fx),
            mix(texel(x0, y1), texel(x1, y1), fx),
            fy,
        )
    };
    let b = e[2] * (nf - 1.0);
    let b0 = b.floor();
    let b1 = (b0 + 1.0).min(nf - 1.0);
    let x = (e[0] * (nf - 1.0) + 0.5) / nf;
    let y = (e[1] * (nf - 1.0) + 0.5) / nf;
    let s0 = bilinear((x + b0) / nf, y);
    let s1 = bilinear((x + b1) / nf, y);
    std::array::from_fn(|c| s0[c] + (s1[c] - s0[c]) * (b - b0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instance::f16_to_f32;

    fn cube(n: u32, f: impl Fn(f32, f32, f32) -> [f32; 3], header: &str) -> String {
        let mut s = format!("# a test LUT\nTITLE \"test\"\n{header}LUT_3D_SIZE {n}\n\n");
        let last = (n - 1) as f32;
        for b in 0..n {
            for g in 0..n {
                for r in 0..n {
                    let v = f(r as f32 / last, g as f32 / last, b as f32 / last);
                    s += &format!("{:.6} {:.6} {:.6}\n", v[0], v[1], v[2]);
                }
            }
        }
        s
    }

    fn texel(t: &TextureSource) -> impl Fn(u32, u32) -> [f32; 3] + '_ {
        move |x, y| {
            let i = ((y * t.width + x) * 8) as usize;
            let c = |k: usize| f16_to_f32(u16::from_le_bytes([t.data[i + 2 * k], t.data[i + 2 * k + 1]]));
            [c(0), c(1), c(2)]
        }
    }

    #[test]
    fn cube_parser_reads_a_strip() {
        let src = cube(4, |r, g, b| [r, g * 0.5, 1.0 - b], "");
        let t = parse_cube(&src).unwrap();
        assert_eq!((t.width, t.height, t.format), (16, 4, wgpu::TextureFormat::Rgba16Float));
        assert_eq!(t.data.len(), 16 * 4 * 8);
        let px = texel(&t);
        // Slice b = 1 (x 4..8), r = 2, g = 3.
        let v = px(4 + 2, 3);
        assert!((v[0] - 2.0 / 3.0).abs() < 1e-3 && (v[1] - 0.5).abs() < 1e-3 && (v[2] - 2.0 / 3.0).abs() < 1e-3);
        // The lookup between texels interpolates the function (linear here: exact).
        let e = [0.3, 0.7, 0.45];
        let out = lookup(e, 4, &px);
        assert!(
            (out[0] - 0.3).abs() < 2e-3 && (out[1] - 0.35).abs() < 2e-3 && (out[2] - 0.55).abs() < 2e-3,
            "{out:?}"
        );
    }

    #[test]
    fn cube_parser_resamples_other_domains() {
        // The table covers 0..2 with v = input / 2: over 0..1 the strip holds t / 2.
        let src = cube(5, |r, g, b| [r, g, b], "DOMAIN_MIN 0 0 0\nDOMAIN_MAX 2 2 2\n");
        let t = parse_cube(&src).unwrap();
        let px = texel(&t);
        let v = px(5 * 4 + 4, 4);
        assert!(
            (v[0] - 0.5).abs() < 2e-3 && (v[1] - 0.5).abs() < 2e-3 && (v[2] - 0.5).abs() < 2e-3,
            "{v:?}"
        );
    }

    #[test]
    fn cube_parser_errors() {
        assert!(matches!(parse_cube("0 0 0\n"), Err(LutError::MissingSize)));
        assert!(matches!(parse_cube("LUT_3D_SIZE 65\n"), Err(LutError::Size(65))));
        assert!(matches!(parse_cube("LUT_3D_SIZE 1\n"), Err(LutError::Size(1))));
        assert!(matches!(
            parse_cube("LUT_3D_SIZE 2\n0 0 0\n1 1 1\n"),
            Err(LutError::Count { expected: 8, found: 2 })
        ));
        assert!(matches!(
            parse_cube("LUT_3D_SIZE 2\n0 0 x\n"),
            Err(LutError::Parse { line: 2, .. })
        ));
        assert!(matches!(
            parse_cube("LUT_3D_SIZE 2\n0 0\n"),
            Err(LutError::Parse { line: 2, .. })
        ));
        assert!(matches!(
            parse_cube("LUT_1D_SIZE 16\n"),
            Err(LutError::Parse { line: 1, .. })
        ));
        assert!(matches!(
            parse_cube("LUT_3D_SIZE abc\n"),
            Err(LutError::Parse { line: 1, .. })
        ));
        let flipped = cube(2, |r, g, b| [r, g, b], "DOMAIN_MIN 1 1 1\nDOMAIN_MAX 0 0 0\n");
        assert!(matches!(parse_cube(&flipped), Err(LutError::Parse { .. })));
        // Unknown keywords and comments are fine.
        let ok = cube(2, |r, g, b| [r, g, b], "LUT_IN_VIDEO_RANGE\n# comment\n");
        assert!(parse_cube(&ok).is_ok());
    }
}
