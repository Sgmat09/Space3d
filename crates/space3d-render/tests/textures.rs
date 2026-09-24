//! Textures on the GPU: generated mip chains, block-compressed formats (BC/ETC2/ASTC when the
//! adapter supports them) and texture atlases via `InstanceSemantic::UvRect`.

mod common;
use common::*;
use space3d_core::prelude::*;
use space3d_render::wgpu;
use space3d_render::*;

/// 64x64 image of alternating one-texel red and blue columns.
fn columns(n: u32) -> ImageData {
    let mut img = ImageData::new(n, n);
    for y in 0..n {
        for x in 0..n {
            img.set(x, y, if x % 2 == 0 { [255, 0, 0, 255] } else { [0, 0, 255, 255] });
        }
    }
    img
}

/// Render a cube (edge 2, scaled by `scale`) textured with `src` in front of a black background;
/// returns the center pixel and the renderer's counters.
fn draw_texture(src: TextureSource, scale: f32) -> Option<([u8; 4], RenderStats, u64)> {
    let mut app = headless_app(64, 64)?;
    let (mesh, tex) = {
        let a = app.resources.expect_mut::<RenderAssets>();
        (a.add_mesh(MeshData::cube(2.0)), a.add_texture(src))
    };
    app.world.spawn((
        Transform::default().with_scale(Vec3::splat(scale)),
        Mesh3d(mesh),
        Material::unlit(Vec4::ONE).with_texture(tex),
    ));
    app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 4.0),
        Camera3d::default().with_clear_color(Vec4::new(0.0, 0.0, 0.0, 1.0)),
    ));
    let img = render_to_image(&mut app).expect("capture");
    let errors = app.resources.expect::<Renderer>().gpu_error_count();
    Some((img.get(32, 32), stats(&app), errors))
}

fn close(a: [u8; 4], b: [u8; 3], tol: u8) -> bool {
    a[..3].iter().zip(b).all(|(x, y)| x.abs_diff(y) <= tol)
}

/// A minified texture samples the GPU-generated mips: one-texel red/blue columns average to
/// the sRGB-correct purple (linear 0.5 → 188). Without mips the sample aliases to one column's
/// color mix instead.
#[test]
fn generated_mips_average_minified_texels() {
    let src = TextureSource::from_image(columns(64));
    // Magnified (a 6-unit cube 1 unit from the camera) with nearest filtering: one texel.
    let mut nearest = src.clone();
    nearest.sampler.filter = FilterMode::Nearest;
    let Some((big, ..)) = draw_texture(nearest, 3.0) else {
        return;
    };
    assert!(
        close(big, [255, 0, 0], 1) || close(big, [0, 0, 255], 1),
        "magnified sample reads one mip-0 texel: {big:?}"
    );
    let (tiny, s, errors) = draw_texture(src.clone(), 0.04).expect("adapter");
    assert_eq!(s.textures_uploaded, 1);
    assert_eq!(errors, 0);
    assert!(
        close(tiny, [188, 0, 188], 3),
        "minified sample should be the mip average: {tiny:?}"
    );
    let without_mips = TextureSource {
        generate_mipmaps: false,
        ..src
    };
    let (aliased, ..) = draw_texture(without_mips, 0.04).expect("adapter");
    assert!(
        !close(aliased, [188, 0, 188], 20),
        "without mips the minified sample must not be the average: {aliased:?}"
    );
}

/// BC7 mode-6 block with both endpoints opaque red and all indices 0.
fn bc7_red() -> [u8; 16] {
    let mut bits = 0u128;
    let mut pos = 0;
    let mut put = |v: u128, n: u32| {
        bits |= (v & ((1u128 << n) - 1)) << pos;
        pos += n;
    };
    put(1 << 6, 7); // mode 6
    for v in [127, 127, 0, 0, 0, 0, 127, 127] {
        put(v, 7); // R0 R1 G0 G1 B0 B1 A0 A1
    }
    put(1, 1); // p-bits
    put(1, 1);
    bits.to_le_bytes()
}

/// A full, tightly packed mip chain of `block` repeated.
fn block_source(format: wgpu::TextureFormat, block: &[u8], w: u32, h: u32, mips: u32) -> TextureSource {
    let (bw, bh) = format.block_dimensions();
    let mut data = Vec::new();
    for m in 0..mips {
        let (mw, mh) = ((w >> m).max(1), (h >> m).max(1));
        for _ in 0..mw.div_ceil(bw) * mh.div_ceil(bh) {
            data.extend_from_slice(block);
        }
    }
    TextureSource {
        width: w,
        height: h,
        layers: 1,
        format,
        mip_level_count: mips,
        data,
        generate_mipmaps: true,
        sampler: SamplerSettings::default(),
    }
}

fn compressed_cases() -> Vec<(wgpu::TextureFormat, Vec<u8>)> {
    vec![
        // BC1: color0 = color1 = pure red (RGB565 0xF800), indices 0.
        (
            wgpu::TextureFormat::Bc1RgbaUnormSrgb,
            vec![0x00, 0xF8, 0x00, 0xF8, 0, 0, 0, 0],
        ),
        (wgpu::TextureFormat::Bc7RgbaUnormSrgb, bc7_red().to_vec()),
        // ETC2 individual mode: base colors R=15 (red), table 0, all pixel indices 0.
        (wgpu::TextureFormat::Etc2Rgb8UnormSrgb, vec![0xFF, 0, 0, 0, 0, 0, 0, 0]),
        // ASTC void-extent block: constant (unorm16) RGBA = (1, 0, 0, 1).
        (
            wgpu::TextureFormat::Astc {
                block: wgpu::AstcBlock::B4x4,
                channel: wgpu::AstcChannel::UnormSrgb,
            },
            vec![
                0xFC, 0xFD, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0, 0xFF, 0xFF,
            ],
        ),
    ]
}

/// Full mip chains of compressed formats upload (block-aligned physical extents for the small
/// mips, including sizes that are not powers of two) and sample the encoded color at mip 0 and
/// at the smallest mips. Formats the adapter lacks are skipped, and so is ETC2 on ANGLE (see
/// below).
#[test]
fn compressed_textures_upload_full_mip_chains() {
    let Some(app) = headless_app(8, 8) else { return };
    let renderer = app.resources.expect::<Renderer>();
    let adapter = &renderer.capabilities().adapter;
    // ANGLE's GLES 3.0 (e.g. `WGPU_BACKEND=gl` with `--features wgpu/angle` on macOS, see
    // `tests/gl.rs`) must report ETC2, and uploads it without error, but samples it as black:
    // a driver limitation the renderer cannot detect (ARCHITECTURE.md, Textures).
    let angle = adapter.backend == wgpu::Backend::Gl && adapter.name.contains("ANGLE");
    let cases: Vec<_> = compressed_cases()
        .into_iter()
        .filter(|(f, _)| {
            let ok = renderer.supports_texture_format(*f);
            if !ok {
                eprintln!("skipping {f:?}: not supported by this adapter");
            }
            let etc2_on_angle = angle && f.required_features() == wgpu::Features::TEXTURE_COMPRESSION_ETC2;
            if ok && etc2_on_angle {
                eprintln!("skipping {f:?}: ANGLE samples ETC2 as black");
            }
            ok && !etc2_on_angle
        })
        .collect();
    drop(app);
    for (format, block) in cases {
        for (w, h) in [(64u32, 64u32), (12, 12), (64, 16), (20, 8)] {
            let full = 32 - w.max(h).leading_zeros();
            let src = block_source(format, &block, w, h, full);
            for scale in [1.0, 0.04] {
                let (px, s, errors) = draw_texture(src.clone(), scale).expect("adapter");
                assert_eq!(s.textures_uploaded, 1, "{format:?} {w}x{h}");
                assert_eq!(errors, 0, "{format:?} {w}x{h}");
                assert!(
                    px[0] >= 240 && px[1] <= 16 && px[2] <= 16,
                    "{format:?} {w}x{h} at scale {scale}: expected red, got {px:?}"
                );
            }
        }
    }
}

/// Regression: a compressed texture whose size is not a whole number of blocks reached
/// `create_texture` and panicked in wgpu's validation. It is now refused like any other bad
/// texture: logged, not uploaded, sampled as white.
#[test]
fn unaligned_compressed_textures_are_refused_not_fatal() {
    let Some(app) = headless_app(8, 8) else { return };
    let format = wgpu::TextureFormat::Bc1RgbaUnormSrgb;
    if !app.resources.expect::<Renderer>().supports_texture_format(format) {
        eprintln!("skipping: BC1 unsupported");
        return;
    }
    drop(app);
    let src = block_source(format, &[0x00, 0xF8, 0x00, 0xF8, 0, 0, 0, 0], 30, 30, 1);
    let (px, s, errors) = draw_texture(src, 1.0).expect("adapter");
    assert_eq!(s.textures_uploaded, 0);
    assert_eq!(errors, 0);
    assert_eq!(px, [255, 255, 255, 255], "missing textures sample white");
}

/// Atlas instance: position, scale and the atlas rect (`uv * zw + xy`) of one sprite.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct AtlasSprite {
    translation: [f32; 3],
    scale: f32,
    uv_rect: [f32; 4],
}

impl InstanceData for AtlasSprite {
    const LAYOUT: &'static InstanceLayout = &InstanceLayout {
        name: "AtlasSprite",
        stride: 32,
        attributes: &[
            InstanceAttribute::new(InstanceSemantic::Translation, wgpu::VertexFormat::Float32x3, 0),
            InstanceAttribute::new(InstanceSemantic::UniformScale, wgpu::VertexFormat::Float32, 12),
            InstanceAttribute::new(InstanceSemantic::UvRect, wgpu::VertexFormat::Float32x4, 16),
        ],
    };
}

/// Two instances of one mesh sample different halves of an atlas through their `UvRect`.
#[test]
fn atlas_rects_select_sub_images_per_instance() {
    let Some(mut app) = headless_app(64, 64) else { return };
    let mut atlas = ImageData::new(8, 4);
    for y in 0..4 {
        for x in 0..8 {
            atlas.set(x, y, if x < 4 { [255, 0, 0, 255] } else { [0, 255, 0, 255] });
        }
    }
    let (mesh, tex) = {
        let a = app.resources.expect_mut::<RenderAssets>();
        let mut src = TextureSource::from_image(atlas);
        src.sampler.filter = FilterMode::Nearest;
        (a.add_mesh(MeshData::cube(1.0)), a.add_texture(src))
    };
    let sprite = |x: f32, u: f32| AtlasSprite {
        translation: [x, 0.0, 0.0],
        scale: 1.0,
        uv_rect: [u, 0.0, 0.5, 1.0],
    };
    let buf = InstanceBuffer::from_vec(vec![sprite(-0.8, 0.0), sprite(0.8, 0.5)], BufferUsage::Static);
    let instances = app.resources.expect_mut::<InstanceBuffers>().insert(buf);
    app.world.spawn((InstancedMesh {
        mesh,
        material: Material::unlit(Vec4::ONE).with_texture(tex),
        instances,
    },));
    spawn_camera(&mut app, 4.0);
    for path in paths_up_to(best_path(&app)) {
        set_path(&mut app, path);
        let img = render_to_image(&mut app).expect("capture");
        assert_eq!(img.get(20, 32), [255, 0, 0, 255], "{path:?}: left instance = left half");
        assert_eq!(
            img.get(44, 32),
            [0, 255, 0, 255],
            "{path:?}: right instance = right half"
        );
    }
}
