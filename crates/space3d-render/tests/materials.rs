//! Material maps and blend modes (graphics spec §5.2 PM1/PM2): exact blend math, alpha masks,
//! normal maps against tilted geometry (per-pixel and vertex tangent frames), ORM packing,
//! occlusion, emissive maps, the opt-in transparent sort, UV transforms and the second UV set,
//! and the extension vertex stream on every draw path.
//!
//! Every test skips without a GPU adapter. `WGPU_BACKEND=gl` (with ANGLE, see `tests/gl.rs`)
//! runs them on the WebGL2-class GL backend.

mod common;
use common::*;
use space3d_core::prelude::*;
use space3d_render::*;

/// Headless app of `w × h` with `samples` MSAA samples.
fn app_with(w: u32, h: u32, samples: u32) -> Option<App> {
    let mut app = App::new();
    app.add_plugin(RenderPlugin::default());
    match pollster::block_on(Renderer::new_headless_with(RendererConfig {
        width: w,
        height: h,
        msaa_samples: samples,
        ..Default::default()
    })) {
        Ok(r) => {
            app.insert_resource(r);
            Some(app)
        }
        Err(e) => {
            eprintln!("skipping GPU test: no adapter available ({e})");
            None
        }
    }
}

fn render(app: &mut App) -> ImageData {
    let img = render_to_image(app).expect("capture");
    assert_eq!(app.resources.expect::<Renderer>().gpu_error_count(), 0, "GPU errors");
    img
}

/// A quad facing +Z (`half` its half-size) with glTF-style UVs: u along +X, v = 0 at the top.
fn quad(half: f32, normal: Vec3) -> MeshData {
    MeshData {
        positions: vec![
            [-half, -half, 0.0],
            [half, -half, 0.0],
            [half, half, 0.0],
            [-half, half, 0.0],
        ],
        normals: vec![normal.normalize().into(); 4],
        uvs: vec![[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]],
        colors: vec![],
        indices: vec![0, 1, 2, 0, 2, 3],
    }
}

/// An orthographic camera at z = 5 looking down −Z, `height` world units tall.
fn ortho_camera(app: &mut App, height: f32, clear: Vec4) -> Entity {
    app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 5.0),
        Camera3d::default()
            .with_clear_color(clear)
            .with_projection(Projection::Orthographic {
                height,
                near: 0.1,
                far: 20.0,
            }),
    ))
}

fn px(img: &ImageData, x: u32, y: u32) -> [u8; 4] {
    img.get(x, y)
}

fn to_u8(linear: f32) -> f32 {
    linear_to_srgb(linear.clamp(0.0, 1.0)) * 255.0
}

fn assert_close(actual: [u8; 4], expected: [f32; 4], tol: f32, what: &str) {
    for c in 0..4 {
        assert!(
            (actual[c] as f32 - expected[c]).abs() <= tol,
            "{what}: channel {c}: {actual:?} vs {expected:?}"
        );
    }
}

/// Five unlit quads over a known clear color, one per blend mode, against the blend equations
/// evaluated on the CPU — over an opaque and over a transparent background.
#[test]
fn blend_modes_composite_exactly() {
    for bg_alpha in [1.0f32, 0.0] {
        let Some(mut app) = app_with(100, 20, 1) else { return };
        let bg = Vec4::new(0.2, 0.3, 0.1, bg_alpha);
        ortho_camera(&mut app, 2.0, bg);
        let mesh = app.resources.expect_mut::<RenderAssets>().add_mesh(quad(0.8, Vec3::Z));
        let c = Vec4::new(0.6, 0.25, 0.5, 0.5);
        let modes = [
            BlendMode::Opaque,
            BlendMode::AlphaMask,
            BlendMode::AlphaBlend,
            BlendMode::Premultiplied,
            BlendMode::Additive,
        ];
        for (i, mode) in modes.into_iter().enumerate() {
            let mut m = Material::unlit(c).with_blend(mode);
            if mode == BlendMode::AlphaMask {
                m.alpha_cutoff = 0.25;
            }
            app.world
                .spawn((Transform::from_xyz(i as f32 * 2.0 - 4.0, 0.0, 0.0), Mesh3d(mesh), m));
        }
        let img = render(&mut app);
        let (rgb, a) = (c.truncate(), c.w);
        let (bgc, ba) = (bg.truncate(), bg.w);
        let expected: [(Vec3, f32); 5] = [
            (rgb, a),                                        // opaque: alpha written as is
            (rgb, 1.0),                                      // mask: passed fragments are opaque
            (rgb * a + bgc * (1.0 - a), a + ba * (1.0 - a)), // SrcAlpha, OneMinusSrcAlpha
            (rgb + bgc * (1.0 - a), a + ba * (1.0 - a)),     // One, OneMinusSrcAlpha
            (rgb * a + bgc, ba),                             // SrcAlpha, One; alpha kept
        ];
        for (i, (e, ea)) in expected.into_iter().enumerate() {
            let x = 10 + i as u32 * 20;
            let want = [to_u8(e.x), to_u8(e.y), to_u8(e.z), ea * 255.0];
            assert_close(
                px(&img, x, 10),
                want,
                1.5,
                &format!("{:?} over alpha {bg_alpha}", modes[i]),
            );
        }
        // Between the quads: the clear color.
        let clear = [to_u8(bgc.x), to_u8(bgc.y), to_u8(bgc.z), ba * 255.0];
        assert_close(px(&img, 0, 10), clear, 1.0, "background");
    }
}

/// A 2 × 1 texture: left texel alpha 0.1, right texel alpha 0.9 (nearest filtering).
fn half_alpha_texture(app: &mut App) -> TextureHandle {
    let mut image = ImageData::new(2, 1);
    image.set(0, 0, [255, 255, 255, 26]);
    image.set(1, 0, [255, 255, 255, 230]);
    let source = TextureSource {
        sampler: SamplerSettings {
            filter: FilterMode::Nearest,
            address: AddressMode::Clamp,
        },
        generate_mipmaps: false,
        ..TextureSource::from_image(image)
    };
    app.resources.expect_mut::<RenderAssets>().add_texture(source)
}

/// `AlphaMask` discards below the cutoff: the discarded half shows what is behind (the mask
/// wrote no depth there), the kept half is opaque and hides it.
#[test]
fn alpha_mask_discards() {
    let Some(mut app) = app_with(40, 20, 1) else { return };
    ortho_camera(&mut app, 2.0, Vec4::new(0.0, 0.0, 1.0, 1.0));
    let tex = half_alpha_texture(&mut app);
    let mesh = app.resources.expect_mut::<RenderAssets>().add_mesh(quad(1.0, Vec3::Z));
    app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 1.0),
        Mesh3d(mesh),
        Material::unlit(Vec4::new(1.0, 0.0, 0.0, 1.0))
            .with_texture(tex)
            .with_alpha_mask(0.5),
    ));
    // Behind it, a green quad.
    app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 0.0),
        Mesh3d(mesh),
        Material::unlit(Vec4::new(0.0, 1.0, 0.0, 1.0)),
    ));
    let img = render(&mut app);
    // (The quads cover x 10..30.)
    assert_eq!(px(&img, 14, 10), [0, 255, 0, 255], "discarded: green behind");
    assert_eq!(px(&img, 26, 10), [255, 0, 0, 255], "kept: opaque red");
    assert_eq!(px(&img, 2, 10), [0, 0, 255, 255], "clear");
    assert_eq!(stats(&app).draw_calls, 2);
}

/// With MSAA the mask becomes alpha to coverage: a smooth alpha ramp resolves to partial
/// coverage around the cutoff instead of a hard, aliased edge; without MSAA the same material
/// discards (only full or empty pixels).
#[test]
fn alpha_to_coverage_with_msaa() {
    let mut partial = [0usize; 2];
    for (i, samples) in [1u32, 4].into_iter().enumerate() {
        let Some(mut app) = app_with(64, 8, samples) else {
            return;
        };
        if app.resources.expect::<Renderer>().capabilities().max_msaa < samples {
            eprintln!("skipping: no {samples}x MSAA");
            return;
        }
        ortho_camera(&mut app, 0.5, Vec4::new(0.0, 0.0, 0.0, 1.0));
        // Alpha ramp 0 → 1 across the quad (linear filtering between 64 texels).
        let mut image = ImageData::new(64, 1);
        for x in 0..64 {
            image.set(x, 0, [255, 255, 255, (x * 4) as u8]);
        }
        let tex = app.resources.expect_mut::<RenderAssets>().add_texture(TextureSource {
            sampler: SamplerSettings {
                filter: FilterMode::Linear,
                address: AddressMode::Clamp,
            },
            generate_mipmaps: false,
            ..TextureSource::from_image(image)
        });
        let mut mesh = quad(1.0, Vec3::Z);
        mesh.positions = vec![[-2.0, -1.0, 0.0], [2.0, -1.0, 0.0], [2.0, 1.0, 0.0], [-2.0, 1.0, 0.0]];
        let mesh = app.resources.expect_mut::<RenderAssets>().add_mesh(mesh);
        app.world.spawn((
            Transform::IDENTITY,
            Mesh3d(mesh),
            Material::unlit(Vec4::ONE).with_texture(tex).with_alpha_mask(0.5),
        ));
        let img = render(&mut app);
        let row: Vec<u8> = (0..64).map(|x| px(&img, x, 4)[0]).collect();
        assert!(
            row[..24].iter().all(|&v| v == 0),
            "{samples}x: low alpha masked: {row:?}"
        );
        assert!(
            row[40..].iter().all(|&v| v == 255),
            "{samples}x: high alpha kept: {row:?}"
        );
        partial[i] = row.iter().filter(|&&v| v != 0 && v != 255).count();
    }
    assert_eq!(partial[0], 0, "1 sample: discard only");
    assert!(partial[1] >= 1, "4 samples: coverage between 0 and 1 around the cutoff");
}

/// Linear RGBA8 normal map of one constant tangent-space normal.
fn normal_map(app: &mut App, n: Vec3) -> TextureHandle {
    let n = n.normalize();
    let e = |v: f32| ((v * 0.5 + 0.5) * 255.0).round() as u8;
    app.resources
        .expect_mut::<RenderAssets>()
        .add_image_linear(ImageData::filled(4, 4, [e(n.x), e(n.y), e(n.z), 255]))
}

/// The tangent-space normal that 8-bit encoding actually stores (what the shader decodes).
fn quantized(n: Vec3) -> Vec3 {
    let n = n.normalize();
    let q = |v: f32| ((v * 0.5 + 0.5) * 255.0).round() / 255.0 * 2.0 - 1.0;
    let (x, y) = (q(n.x), q(n.y));
    Vec3::new(x, y, (1.0 - x * x - y * y).max(0.0).sqrt()).normalize()
}

/// Two quads under the same lights: one flat with a normal map of the tangent-space normal
/// `n_ts`, one whose geometric normal is that normal (tangent +u = +X, bitangent −v = +Y,
/// glTF), both turned by `turn`. They must shade alike.
fn normal_map_scene(tangents: bool, turn: Quat) -> Option<(ImageData, ImageData)> {
    let n_ts = Vec3::new(0.45, -0.3, 1.0);
    let mut images = Vec::new();
    for mapped in [true, false] {
        let mut app = app_with(32, 32, 1)?;
        app.world.spawn((
            Transform::from_xyz(0.0, 0.0, 5.0),
            Camera3d::default().with_clear_color(Vec4::new(0.0, 0.0, 0.0, 1.0)),
        ));
        app.world.spawn((
            Transform::IDENTITY.looking_at(Vec3::new(0.3, -0.2, -1.0), Vec3::Y),
            DirectionalLight {
                color: Vec3::ONE,
                intensity: 0.8,
            },
        ));
        app.insert_resource(AmbientLight {
            color: Vec3::ONE,
            intensity: 0.0,
        });
        let material = Material::color(Vec4::new(0.7, 0.6, 0.5, 1.0)).with_metallic_roughness(0.0, 0.5);
        let (mesh, material) = if mapped {
            let map = normal_map(&mut app, n_ts);
            let data = quad(2.0, Vec3::Z);
            let a = app.resources.expect_mut::<RenderAssets>();
            let mesh = if tangents {
                a.add_mesh_with(data, MeshExtras::new().with_tangents(vec![[1.0, 0.0, 0.0, 1.0]; 4]))
            } else {
                a.add_mesh(data)
            };
            (mesh, material.with_normal_map(map, 1.0))
        } else {
            let mesh = app
                .resources
                .expect_mut::<RenderAssets>()
                .add_mesh(quad(2.0, quantized(n_ts)));
            (mesh, material)
        };
        app.world
            .spawn((Transform::IDENTITY.with_rotation(turn), Mesh3d(mesh), material));
        images.push(render(&mut app));
    }
    let flat = images.pop()?;
    Some((images.pop()?, flat))
}

fn assert_same_shading(mapped: &ImageData, flat: &ImageData, what: &str) {
    let (mut lit, mut worst) = (0, 0u8);
    for y in 8..24 {
        for x in 8..24 {
            let (a, b) = (px(mapped, x, y), px(flat, x, y));
            if b[..3] != [0, 0, 0] {
                lit += 1;
            }
            for c in 0..3 {
                worst = worst.max(a[c].abs_diff(b[c]));
            }
        }
    }
    assert!(lit > 200, "{what}: the quads are lit ({lit} px)");
    assert!(worst <= 3, "{what}: normal map vs tilted normal differ by {worst}");
}

/// The per-pixel (derivative) tangent frame: a flat quad with a constant normal map shades
/// like a quad whose normal is the mapped one — axis-aligned, and turned (the frame follows
/// the surface).
#[test]
fn normal_map_matches_tilted_geometry() {
    for turn in [Quat::IDENTITY, Quat::from_rotation_y(0.5) * Quat::from_rotation_z(0.7)] {
        let Some((mapped, flat)) = normal_map_scene(false, turn) else {
            return;
        };
        assert_same_shading(&mapped, &flat, &format!("derivative frame, turn {turn}"));
    }
}

/// The vertex tangent frame (extension stream): same result as tilted geometry.
#[test]
fn normal_map_matches_tilted_geometry_with_vertex_tangents() {
    for turn in [Quat::IDENTITY, Quat::from_rotation_x(-0.4) * Quat::from_rotation_z(2.0)] {
        let Some((mapped, flat)) = normal_map_scene(true, turn) else {
            return;
        };
        assert_same_shading(&mapped, &flat, &format!("vertex tangents, turn {turn}"));
    }
}

/// A lit sphere with the maps `with` adds, under a sun and ambient light.
fn lit_sphere(with: impl FnOnce(&mut App, Material) -> Material, ambient: f32, sun: bool) -> Option<ImageData> {
    let mut app = app_with(48, 48, 1)?;
    app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 4.0),
        Camera3d::default().with_clear_color(Vec4::new(0.0, 0.0, 0.0, 1.0)),
    ));
    if sun {
        app.world.spawn((
            Transform::IDENTITY.looking_at(Vec3::new(-0.4, -0.5, -1.0), Vec3::Y),
            DirectionalLight {
                color: Vec3::ONE,
                intensity: 1.2,
            },
        ));
    }
    app.insert_resource(AmbientLight {
        color: Vec3::ONE,
        intensity: ambient,
    });
    let mesh = app
        .resources
        .expect_mut::<RenderAssets>()
        .add_mesh(MeshData::sphere(1.2, 32, 16));
    let base = Material::color(Vec4::new(0.8, 0.7, 0.6, 1.0)).with_metallic_roughness(1.0, 1.0);
    let material = with(&mut app, base);
    app.world.spawn((Transform::IDENTITY, Mesh3d(mesh), material));
    Some(render(&mut app))
}

fn linear_image(app: &mut App, rgba: [u8; 4]) -> TextureHandle {
    app.resources
        .expect_mut::<RenderAssets>()
        .add_image_linear(ImageData::filled(4, 4, rgba))
}

/// One ORM texture (R occlusion, G roughness, B metallic) renders exactly like separate
/// metallic-roughness and occlusion maps with the same channels.
#[test]
fn orm_packed_equals_separate_maps() {
    let orm = [100, 140, 60, 255];
    let Some(packed) = lit_sphere(
        |app, m| {
            let t = linear_image(app, orm);
            m.with_orm_map(t)
        },
        0.3,
        true,
    ) else {
        return;
    };
    let separate = lit_sphere(
        |app, m| {
            let mr = linear_image(app, [0, orm[1], orm[2], 255]);
            let occlusion = linear_image(app, [orm[0], 0, 0, 255]);
            m.with_metallic_roughness_map(mr).with_occlusion_map(occlusion, 1.0)
        },
        0.3,
        true,
    )
    .unwrap();
    assert_eq!(packed, separate);
    // And the map matters: roughness 0.55 and metal 0.24 instead of 1 and 1.
    let plain = lit_sphere(|_, m| m, 0.3, true).unwrap();
    assert!(diff_fraction(&packed, &plain, 4) > 0.2);
}

/// Occlusion darkens ambient light only: under a sun alone it changes nothing; under ambient
/// alone it scales by `1 + strength × (occlusion − 1)`.
#[test]
fn occlusion_only_darkens_ambient() {
    let occluded = |strength: f32| {
        move |app: &mut App, m: Material| {
            let t = linear_image(app, [64, 255, 255, 255]);
            m.with_occlusion_map(t, strength)
        }
    };
    let Some(sun_plain) = lit_sphere(|_, m| m, 0.0, true) else {
        return;
    };
    let sun_occluded = lit_sphere(occluded(1.0), 0.0, true).unwrap();
    assert_eq!(sun_plain, sun_occluded, "direct light ignores occlusion");
    let ambient = lit_sphere(|_, m| m, 0.6, false).unwrap();
    let occ = 64.0 / 255.0;
    for strength in [1.0, 0.5] {
        let img = lit_sphere(occluded(strength), 0.6, false).unwrap();
        let (a, b) = (px(&ambient, 24, 24), px(&img, 24, 24));
        let factor = 1.0 + strength * (occ - 1.0);
        for c in 0..3 {
            let expected = to_u8(srgb_to_linear(a[c] as f32 / 255.0) * factor);
            assert!(
                (b[c] as f32 - expected).abs() <= 1.5,
                "strength {strength}: {b:?} vs ambient {a:?} × {factor}"
            );
        }
    }
}

/// The emissive map (sRGB) multiplies the emissive factor.
#[test]
fn emissive_map_times_factor() {
    let Some(mut app) = app_with(16, 16, 1) else { return };
    ortho_camera(&mut app, 2.0, Vec4::new(0.0, 0.0, 0.0, 1.0));
    app.insert_resource(AmbientLight {
        color: Vec3::ONE,
        intensity: 0.0,
    });
    let map = [255, 128, 64, 255];
    let a = app.resources.expect_mut::<RenderAssets>();
    let tex = a.add_image(ImageData::filled(4, 4, map));
    let mesh = a.add_mesh(quad(2.0, Vec3::Z));
    let factor = Vec3::new(0.8, 0.5, 0.25);
    app.world.spawn((
        Transform::IDENTITY,
        Mesh3d(mesh),
        Material::color(Vec4::new(0.0, 0.0, 0.0, 1.0)).with_emissive_map(tex, factor),
    ));
    let img = render(&mut app);
    let e = |i: usize| to_u8(srgb_to_linear(map[i] as f32 / 255.0) * factor[i]);
    assert_close(px(&img, 8, 8), [e(0), e(1), e(2), 255.0], 1.5, "emissive map × factor");
}

/// Two overlapping half-transparent quads of one batch (the left texel of their texture is
/// red, the right one green): the near quad shows red where they overlap, the far one green.
fn sorted_scene(near_first: bool, sort: TransparentSort, path: Option<DrawPath>) -> Option<ImageData> {
    let mut app = app_with(40, 20, 1)?;
    if let Some(path) = path {
        set_path(&mut app, path);
    }
    app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 6.0),
        Camera3d::default().with_clear_color(Vec4::new(0.0, 0.0, 0.0, 1.0)),
    ));
    app.resources.expect_mut::<LightingSettings>().transparent_sort = sort;
    let mut image = ImageData::new(2, 1);
    image.set(0, 0, [255, 0, 0, 128]);
    image.set(1, 0, [0, 255, 0, 128]);
    let a = app.resources.expect_mut::<RenderAssets>();
    let tex = a.add_texture(TextureSource {
        sampler: SamplerSettings {
            filter: FilterMode::Nearest,
            address: AddressMode::Clamp,
        },
        generate_mipmaps: false,
        ..TextureSource::from_image(image)
    });
    let mesh = a.add_mesh(quad(1.0, Vec3::Z));
    let material = Material::unlit(Vec4::ONE)
        .with_texture(tex)
        .with_blend(BlendMode::AlphaBlend);
    let near = (Transform::from_xyz(0.5, 0.0, 0.0), Mesh3d(mesh), material.clone());
    let far = (Transform::from_xyz(-0.5, 0.0, -1.0), Mesh3d(mesh), material);
    if near_first {
        app.world.spawn(near);
        app.world.spawn(far);
    } else {
        app.world.spawn(far);
        app.world.spawn(near);
    }
    Some(render(&mut app))
}

/// `TransparentSort::None` (the default) keeps batch (spawn) order; `PerInstance` sorts the
/// instances of a batch back to front per view, on every draw path.
#[test]
fn transparent_sort_opt_in_only() {
    assert_eq!(LightingSettings::default().transparent_sort, TransparentSort::None);
    let Some(far_first) = sorted_scene(false, TransparentSort::None, None) else {
        return;
    };
    let near_first = sorted_scene(true, TransparentSort::None, None).unwrap();
    // The overlap: red over green (correct) vs green over red.
    let overlap = |img: &ImageData| px(img, 20, 10);
    let (right, wrong) = (overlap(&far_first), overlap(&near_first));
    assert!(right[0] > right[1] && wrong[1] > wrong[0], "{right:?} {wrong:?}");
    // Every draw path sorts the same way.
    let sort = TransparentSort::PerInstance { max_instances: 64 };
    for path in paths_up_to(best_path(&app_with(4, 4, 1).unwrap())) {
        for near_first in [false, true] {
            assert_eq!(
                sorted_scene(near_first, sort, Some(path)).unwrap(),
                far_first,
                "sorted back to front ({path:?}, near spawned first: {near_first})"
            );
        }
    }
    // Batches larger than the limit keep batch order.
    let small = TransparentSort::PerInstance { max_instances: 1 };
    assert_eq!(sorted_scene(true, small, None).unwrap(), near_first);
}

/// `KHR_texture_transform` (`Material::uv_transform`): an offset of half the texture swaps the
/// halves of a two-texel texture (Repeat), for the base color and for maps alike.
#[test]
fn uv_transform_moves_every_map() {
    let Some(mut app) = app_with(40, 20, 1) else { return };
    ortho_camera(&mut app, 2.0, Vec4::new(0.0, 0.0, 0.0, 1.0));
    let mut image = ImageData::new(2, 1);
    image.set(0, 0, [255, 0, 0, 255]);
    image.set(1, 0, [0, 0, 255, 255]);
    let a = app.resources.expect_mut::<RenderAssets>();
    let tex = a.add_texture(TextureSource {
        sampler: SamplerSettings {
            filter: FilterMode::Nearest,
            address: AddressMode::Repeat,
        },
        generate_mipmaps: false,
        ..TextureSource::from_image(image)
    });
    let mesh = a.add_mesh(quad(1.0, Vec3::Z));
    let shift = TextureTransform {
        offset: Vec2::new(0.5, 0.0),
        ..TextureTransform::IDENTITY
    };
    app.world.spawn((
        Transform::from_xyz(-1.0, 0.0, 0.0),
        Mesh3d(mesh),
        Material::unlit(Vec4::ONE).with_texture(tex).with_uv_transform(shift),
    ));
    app.world.spawn((
        Transform::from_xyz(1.0, 0.0, 0.0),
        Mesh3d(mesh),
        Material::unlit(Vec4::new(0.0, 0.0, 0.0, 1.0))
            .with_emissive_map(tex, Vec3::ONE)
            .with_uv_transform(shift),
    ));
    let img = render(&mut app);
    for x0 in [0, 20] {
        assert_eq!(px(&img, x0 + 5, 10)[..3], [0, 0, 255], "left half now blue ({x0})");
        assert_eq!(px(&img, x0 + 15, 10)[..3], [255, 0, 0], "right half now red ({x0})");
    }
}

/// A 2 × 1 texture, left texel red, right texel blue (nearest).
fn red_blue(app: &mut App) -> TextureHandle {
    let mut image = ImageData::new(2, 1);
    image.set(0, 0, [255, 0, 0, 255]);
    image.set(1, 0, [0, 0, 255, 255]);
    app.resources.expect_mut::<RenderAssets>().add_texture(TextureSource {
        sampler: SamplerSettings {
            filter: FilterMode::Nearest,
            address: AddressMode::Clamp,
        },
        generate_mipmaps: false,
        ..TextureSource::from_image(image)
    })
}

/// The extension-stream scene: a normal-mapped quad with vertex tangents and an emissive map on
/// its second UV set, drawn as `copies` instances in a row. With `filler`, other meshes are
/// uploaded first, so the quad sits past the start of the arena (`base_vertex > 0`: slot 2 is
/// rebound with slot 0 where `BASE_VERTEX` is missing).
fn ext_scene(filler: bool, path: Option<DrawPath>, copies: u32) -> Option<ImageData> {
    let mut app = app_with(64, 32, 1)?;
    if let Some(path) = path {
        set_path(&mut app, path);
    }
    ortho_camera(&mut app, 2.0, Vec4::new(0.0, 0.0, 0.0, 1.0));
    app.world.spawn((
        Transform::IDENTITY.looking_at(Vec3::new(0.3, -0.2, -1.0), Vec3::Y),
        DirectionalLight {
            color: Vec3::ONE,
            intensity: 0.8,
        },
    ));
    if filler {
        let a = app.resources.expect_mut::<RenderAssets>();
        let cube = a.add_mesh(MeshData::cube(1.0));
        a.add_mesh(MeshData::sphere(1.0, 12, 6));
        app.world
            .spawn((Transform::from_xyz(100.0, 0.0, 0.0), Mesh3d(cube), Material::default()));
    }
    let map = normal_map(&mut app, Vec3::new(0.5, 0.2, 1.0));
    let tex = red_blue(&mut app);
    let n = 4;
    let extras = MeshExtras::new()
        .with_tangents(vec![[1.0, 0.0, 0.0, 1.0]; n])
        .with_uvs1(vec![[0.75, 0.5]; n]);
    let mesh = app
        .resources
        .expect_mut::<RenderAssets>()
        .add_mesh_with(quad(0.45, Vec3::Z), extras);
    let material = Material::color(Vec4::new(0.6, 0.6, 0.6, 1.0))
        .with_normal_map(map, 1.0)
        .with_emissive_map(tex, Vec3::splat(0.3))
        .with_uv1_maps(MapMask::EMISSIVE);
    for i in 0..copies {
        app.world.spawn((
            Transform::from_xyz(-1.5 + i as f32, 0.0, 0.0),
            Mesh3d(mesh),
            material.clone(),
        ));
    }
    Some(render(&mut app))
}

/// The extension stream draws the same on every draw path (direct, indirect, multi-draw: the
/// indirect args' `base_vertex` applies to slot 2 too) and wherever the mesh sits in the arena.
#[test]
fn ext_stream_on_every_draw_path() {
    let Some(reference) = ext_scene(false, None, 4) else {
        return;
    };
    // The emissive map reads the second UV set (u = 0.75: blue), not UV0 (red on the left).
    let p = px(&reference, 6, 16);
    assert!(p[2] > p[0] + 40, "second UV set: {p:?}");
    let best = best_path(&app_with(4, 4, 1).unwrap());
    for path in paths_up_to(best) {
        for filler in [false, true] {
            let img = ext_scene(filler, Some(path), 4).unwrap();
            assert_eq!(img, reference, "{path:?}, filler {filler}");
        }
    }
}

/// Instances of a layout with more than 10 locations have no room for the extension stream:
/// they draw ext-less (per-pixel tangent frame, UV0 for second-UV-set maps) without errors.
#[test]
fn ext_disabled_past_10_instance_locations() {
    #[repr(C)]
    #[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
    struct Wide {
        model: [f32; 16],
        color: [f32; 4],
        custom: [[f32; 4]; 6],
    }
    impl InstanceData for Wide {
        const LAYOUT: &'static InstanceLayout = &InstanceLayout {
            name: "Wide",
            stride: 176,
            attributes: &[
                InstanceAttribute::new(InstanceSemantic::ModelMatrix, wgpu::VertexFormat::Float32x4, 0),
                InstanceAttribute::new(InstanceSemantic::Color, wgpu::VertexFormat::Float32x4, 64),
                InstanceAttribute::new(InstanceSemantic::Custom(0), wgpu::VertexFormat::Float32x4, 80),
                InstanceAttribute::new(InstanceSemantic::Custom(1), wgpu::VertexFormat::Float32x4, 96),
                InstanceAttribute::new(InstanceSemantic::Custom(2), wgpu::VertexFormat::Float32x4, 112),
                InstanceAttribute::new(InstanceSemantic::Custom(3), wgpu::VertexFormat::Float32x4, 128),
                InstanceAttribute::new(InstanceSemantic::Custom(4), wgpu::VertexFormat::Float32x4, 144),
                InstanceAttribute::new(InstanceSemantic::Custom(5), wgpu::VertexFormat::Float32x4, 160),
            ],
        };
    }
    let mut images = Vec::new();
    for wide in [true, false] {
        let Some(mut app) = app_with(32, 32, 1) else { return };
        ortho_camera(&mut app, 2.0, Vec4::new(0.0, 0.0, 0.0, 1.0));
        app.world.spawn((
            Transform::IDENTITY.looking_at(Vec3::new(0.3, -0.2, -1.0), Vec3::Y),
            DirectionalLight {
                color: Vec3::ONE,
                intensity: 0.8,
            },
        ));
        let map = normal_map(&mut app, Vec3::new(0.5, 0.2, 1.0));
        let tex = red_blue(&mut app);
        let material = Material::color(Vec4::new(0.6, 0.6, 0.6, 1.0))
            .with_normal_map(map, 1.0)
            .with_emissive_map(tex, Vec3::splat(0.3))
            .with_uv1_maps(MapMask::EMISSIVE);
        let a = app.resources.expect_mut::<RenderAssets>();
        if wide {
            let extras = MeshExtras::new()
                .with_tangents(vec![[1.0, 0.0, 0.0, 1.0]; 4])
                .with_uvs1(vec![[0.75, 0.5]; 4]);
            let mesh = a.add_mesh_with(quad(0.9, Vec3::Z), extras);
            let instance = Wide {
                model: Mat4::IDENTITY.to_cols_array(),
                color: [1.0; 4],
                custom: [[0.0; 4]; 6],
            };
            let instances = app
                .resources
                .expect_mut::<InstanceBuffers>()
                .insert(InstanceBuffer::from_vec(vec![instance], BufferUsage::Static));
            app.world.spawn((InstancedMesh {
                mesh,
                material,
                instances,
            },));
        } else {
            // What the wide layout falls back to: no tangents, UV0.
            let mesh = a.add_mesh(quad(0.9, Vec3::Z));
            app.world
                .spawn((Transform::IDENTITY, Mesh3d(mesh), material.with_uv1_maps(MapMask::NONE)));
        }
        images.push(render(&mut app));
    }
    assert_eq!(images[0], images[1]);
}

/// The extension stream survives arena growth (copied with the vertex buffer) and mesh
/// updates: `update_mesh` without extras clears it (per-pixel frame, UV0).
#[test]
fn ext_stream_follows_arena_growth_and_updates() {
    let Some(mut app) = app_with(32, 32, 1) else { return };
    ortho_camera(&mut app, 2.0, Vec4::new(0.0, 0.0, 0.0, 1.0));
    let tex = red_blue(&mut app);
    let extras = MeshExtras::new().with_uvs1(vec![[0.75, 0.5]; 4]);
    let mesh = app
        .resources
        .expect_mut::<RenderAssets>()
        .add_mesh_with(quad(0.9, Vec3::Z), extras);
    app.world.spawn((
        Transform::IDENTITY,
        Mesh3d(mesh),
        Material::unlit(Vec4::new(0.0, 0.0, 0.0, 1.0))
            .with_emissive_map(tex, Vec3::ONE)
            .with_uv1_maps(MapMask::EMISSIVE),
    ));
    let before = render(&mut app);
    assert_eq!(px(&before, 8, 16)[..3], [0, 0, 255], "UV1 everywhere: blue");
    // Grow the vertex arena (and with it the extension stream) well past its first capacity.
    let (capacity, _) = app.resources.expect::<RenderAssets>().arena_usage();
    for _ in 0..8 {
        app.resources
            .expect_mut::<RenderAssets>()
            .add_mesh(MeshData::sphere(1.0, 64, 32));
    }
    let after = render(&mut app);
    assert!(app.resources.expect::<RenderAssets>().arena_usage().0 > capacity + 8 * 2000);
    assert_eq!(after, before, "the extension data was copied with the arena");
    // Without extras the quad samples UV0: red left, blue right.
    app.resources
        .expect_mut::<RenderAssets>()
        .update_mesh(mesh, quad(0.9, Vec3::Z));
    let updated = render(&mut app);
    assert_eq!(px(&updated, 8, 16)[..3], [255, 0, 0]);
    assert_eq!(px(&updated, 24, 16)[..3], [0, 0, 255]);
}

/// A static scene with every map, the extension stream and a sorted transparent batch uploads
/// nothing once warm (texture sets resolved, uniforms and sorted instances diffed), creates no
/// texture and reallocates nothing.
#[test]
fn static_scene_with_maps_uploads_nothing() {
    let Some(mut app) = app_with(64, 64, 1) else { return };
    app.resources.expect_mut::<LightingSettings>().transparent_sort =
        TransparentSort::PerInstance { max_instances: 64 };
    app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 6.0),
        Camera3d::default().with_clear_color(Vec4::new(0.0, 0.0, 0.0, 1.0)),
    ));
    spawn_sun(&mut app);
    let map = normal_map(&mut app, Vec3::new(0.3, 0.1, 1.0));
    let orm = linear_image(&mut app, [200, 128, 255, 255]);
    let tex = red_blue(&mut app);
    let extras = MeshExtras::new()
        .with_tangents(vec![[1.0, 0.0, 0.0, 1.0]; 4])
        .with_uvs1(vec![[0.75, 0.5]; 4]);
    let a = app.resources.expect_mut::<RenderAssets>();
    let mesh = a.add_mesh_with(quad(0.4, Vec3::Z), extras);
    let material = Material::color(Vec4::ONE)
        .with_normal_map(map, 1.0)
        .with_orm_map(orm)
        .with_emissive_map(tex, Vec3::splat(0.5))
        .with_uv1_maps(MapMask::EMISSIVE);
    for i in 0..6 {
        app.world.spawn((
            Transform::from_xyz(i as f32 - 2.5, 0.8, 0.0),
            Mesh3d(mesh),
            material.clone(),
        ));
        app.world.spawn((
            Transform::from_xyz(i as f32 * 0.3 - 0.8, -0.8, -(i as f32) * 0.2),
            Mesh3d(mesh),
            Material::unlit(Vec4::new(0.2, 0.9, 0.4, 0.5)).with_blend(BlendMode::AlphaBlend),
        ));
    }
    for _ in 0..4 {
        render(&mut app);
    }
    for _ in 0..3 {
        render(&mut app);
        let s = stats(&app);
        assert_eq!(
            (
                s.bytes_uploaded,
                s.textures_created,
                s.cpu_reallocations,
                s.buffer_reallocations
            ),
            (0, 0, 0, 0),
            "{s:?}"
        );
    }
}
