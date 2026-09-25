//! Shared helpers for GPU integration tests. Every test calls [`headless_app`] first and
//! returns early (printing a message) when no GPU adapter is available.
#![allow(dead_code)]

use space3d_core::prelude::*;
use space3d_render::*;

/// App with `RenderPlugin` and a headless renderer, or `None` (test should skip).
pub fn headless_app(width: u32, height: u32) -> Option<App> {
    let mut app = App::new();
    app.add_plugin(RenderPlugin::default());
    match pollster::block_on(Renderer::new_headless(width, height)) {
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

/// Camera at `z` looking down -Z with a pure-blue clear color.
pub fn spawn_camera(app: &mut App, z: f32) -> Entity {
    app.world.spawn((
        Transform::from_xyz(0.0, 0.0, z),
        Camera3d::default().with_clear_color(Vec4::new(0.0, 0.0, 1.0, 1.0)),
    ))
}

pub fn spawn_sun(app: &mut App) {
    app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 0.0).looking_at(Vec3::new(-0.3, -1.0, -0.6), Vec3::Y),
        DirectionalLight {
            color: Vec3::ONE,
            intensity: 2.0,
        },
    ));
}

pub const CLEAR: [u8; 4] = [0, 0, 255, 255];

pub fn set_path(app: &mut App, path: DrawPath) {
    app.resources.expect_mut::<RenderSettings>().draw_path = Some(path);
}

pub fn stats(app: &App) -> RenderStats {
    app.resources.expect::<RenderStats>().clone()
}

pub fn best_path(app: &App) -> DrawPath {
    app.resources.expect::<Renderer>().capabilities().best_draw_path
}

/// Every draw path the adapter supports, `CpuDirect` first.
pub fn paths_up_to(best: DrawPath) -> Vec<DrawPath> {
    [
        DrawPath::CpuDirect,
        DrawPath::GpuIndirect,
        DrawPath::GpuMultiDraw,
        DrawPath::GpuMultiDrawCount,
    ]
    .into_iter()
    .filter(|p| *p <= best)
    .collect()
}

/// Fraction of pixels whose max channel difference exceeds `tol`.
pub fn diff_fraction(a: &ImageData, b: &ImageData, tol: u8) -> f64 {
    assert_eq!((a.width, a.height), (b.width, b.height));
    let bad = a
        .pixels
        .chunks(4)
        .zip(b.pixels.chunks(4))
        .filter(|(p, q)| p.iter().zip(q.iter()).any(|(x, y)| x.abs_diff(*y) > tol))
        .count();
    bad as f64 / (a.width * a.height) as f64
}

/// Pixels whose RGB is exactly `rgb`.
pub fn count_rgb(img: &ImageData, rgb: [u8; 3]) -> usize {
    img.pixels.chunks(4).filter(|p| p[..3] == rgb).count()
}

/// Portability scenes: each exercises something downlevel backends (WebGL2/GLES) handle
/// differently, and lists colors (unlit, exact) that must each cover a visible area.
pub mod scenes {
    use space3d_core::prelude::*;
    use space3d_render::*;

    pub type Scene = fn(&mut App) -> &'static [[u8; 3]];

    fn camera(app: &mut App) {
        app.world.spawn((
            Transform::from_xyz(0.0, 0.0, 6.0),
            Camera3d::default().with_clear_color(Vec4::new(0.0, 0.0, 1.0, 1.0)),
        ));
    }

    /// Several meshes in the arena (`base_vertex > 0`), ECS and explicit instances.
    pub fn meshes(app: &mut App) -> &'static [[u8; 3]] {
        camera(app);
        let a = app.resources.expect_mut::<RenderAssets>();
        let cube = a.add_mesh(MeshData::cube(1.2));
        let sphere = a.add_mesh(MeshData::sphere(0.8, 24, 12));
        let plane = a.add_mesh(MeshData::plane(1.0));
        let unlit = |r, g, b| Material::unlit(Vec4::new(r, g, b, 1.0));
        app.world
            .spawn((Transform::from_xyz(-2.0, 0.0, 0.0), Mesh3d(cube), unlit(1.0, 0.0, 0.0)));
        app.world
            .spawn((Transform::from_xyz(0.0, 0.0, 0.0), Mesh3d(sphere), unlit(0.0, 1.0, 0.0)));
        app.world.spawn((
            Transform::from_xyz(2.0, 0.0, 0.0).with_rotation(Quat::from_rotation_x(1.2)),
            Mesh3d(plane),
            unlit(1.0, 1.0, 0.0),
        ));
        let data = (0..5)
            .map(|i| {
                InstanceTransform::from_trs(Vec3::new(i as f32 - 2.0, -1.6, 0.0), Quat::IDENTITY, Vec3::splat(0.4))
            })
            .collect();
        let instances = app
            .resources
            .expect_mut::<InstanceBuffers>()
            .insert(InstanceBuffer::from_vec(data, BufferUsage::Static));
        app.world.spawn((InstancedMesh {
            mesh: sphere,
            material: unlit(1.0, 1.0, 1.0),
            instances,
        },));
        &[[255, 0, 0], [0, 255, 0], [255, 255, 0], [255, 255, 255]]
    }

    /// A single-layer image texture.
    pub fn textured(app: &mut App) -> &'static [[u8; 3]] {
        camera(app);
        let a = app.resources.expect_mut::<RenderAssets>();
        let cube = a.add_mesh(MeshData::cube(3.0));
        let tex = a.add_image(ImageData::checker(16, 4, [255, 0, 0, 255], [0, 255, 0, 255]));
        app.world.spawn((
            Transform::default(),
            Mesh3d(cube),
            Material::unlit(Vec4::ONE).with_texture(tex),
        ));
        &[[255, 0, 0], [0, 255, 0]]
    }

    /// A render target sampled by a material in another view.
    pub fn render_target(app: &mut App) -> &'static [[u8; 3]] {
        camera(app);
        let a = app.resources.expect_mut::<RenderAssets>();
        let monitor = a.add_mesh(MeshData::cube(3.0));
        let cube = a.add_mesh(MeshData::cube(1.0));
        let rt = a.add_render_target(32, 32);
        app.world.spawn((
            Transform::default(),
            Mesh3d(monitor),
            Material::unlit(Vec4::ONE).with_texture(rt),
        ));
        app.world.spawn((
            Transform::from_xyz(50.0, 0.0, 0.0),
            Mesh3d(cube),
            Material::unlit(Vec4::new(1.0, 0.0, 1.0, 1.0)),
        ));
        app.world.spawn((
            Transform::from_xyz(50.0, 0.0, 2.0),
            Camera3d {
                order: -1,
                target: RenderTarget::Texture(rt),
                ..Camera3d::default().with_clear_color(Vec4::new(1.0, 1.0, 0.0, 1.0))
            },
        ));
        &[[255, 255, 0], [255, 0, 255]]
    }

    /// Minified (generated-mip) samples of both layers of a 2-layer texture and of a
    /// single-layer texture.
    pub fn array_mips(app: &mut App) -> &'static [[u8; 3]] {
        camera(app);
        let layers = [
            ImageData::filled(64, 64, [255, 0, 0, 255]),
            ImageData::filled(64, 64, [0, 255, 0, 255]),
        ];
        let a = app.resources.expect_mut::<RenderAssets>();
        let cube = a.add_mesh(MeshData::cube(1.0));
        let array = a.add_texture(TextureSource::from_layers(&layers).expect("equal sizes"));
        let single = a.add_image(ImageData::filled(64, 64, [255, 255, 0, 255]));
        let data = vec![
            InstancePacked::new(Vec3::new(-1.5, 0.0, 0.0), Quat::IDENTITY, 0.5, Vec4::ONE, 0),
            InstancePacked::new(Vec3::new(0.0, 0.0, 0.0), Quat::IDENTITY, 0.5, Vec4::ONE, 1),
        ];
        let instances = app
            .resources
            .expect_mut::<InstanceBuffers>()
            .insert(InstanceBuffer::from_vec(data, BufferUsage::Static));
        app.world.spawn((InstancedMesh {
            mesh: cube,
            material: Material::unlit(Vec4::ONE).with_texture(array),
            instances,
        },));
        app.world.spawn((
            Transform::from_xyz(1.5, 0.0, 0.0).with_scale(Vec3::splat(0.5)),
            Mesh3d(cube),
            Material::unlit(Vec4::ONE).with_texture(single),
        ));
        &[[255, 0, 0], [0, 255, 0], [255, 255, 0]]
    }

    /// A texture array of six square layers: wgpu's GL backend creates such textures as cube
    /// maps, which a `sampler2DArray` cannot read.
    pub fn six_layers(app: &mut App) -> &'static [[u8; 3]] {
        const COLORS: [[u8; 3]; 6] = [
            [255, 0, 0],
            [0, 255, 0],
            [255, 255, 0],
            [255, 0, 255],
            [0, 255, 255],
            [255, 255, 255],
        ];
        camera(app);
        let layers = COLORS.map(|[r, g, b]| ImageData::filled(16, 16, [r, g, b, 255]));
        let a = app.resources.expect_mut::<RenderAssets>();
        let cube = a.add_mesh(MeshData::cube(1.0));
        let array = a.add_texture(TextureSource::from_layers(&layers).expect("equal sizes"));
        let data = (0..6)
            .map(|i| {
                let position = Vec3::new((i % 3) as f32 * 2.0 - 2.0, if i < 3 { 1.0 } else { -1.0 }, 0.0);
                InstancePacked::new(position, Quat::IDENTITY, 0.8, Vec4::ONE, i)
            })
            .collect();
        let instances = app
            .resources
            .expect_mut::<InstanceBuffers>()
            .insert(InstanceBuffer::from_vec(data, BufferUsage::Static));
        app.world.spawn((InstancedMesh {
            mesh: cube,
            material: Material::unlit(Vec4::ONE).with_texture(array),
            instances,
        },));
        &COLORS
    }

    /// Instance colors in `Unorm8x4Bgra`.
    #[repr(C)]
    #[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
    pub struct BgraInstance {
        pub position: [f32; 3],
        pub bgra: [u8; 4],
    }
    impl InstanceData for BgraInstance {
        const LAYOUT: &'static InstanceLayout = &InstanceLayout {
            name: "BgraInstance",
            stride: 16,
            attributes: &[
                InstanceAttribute::new(InstanceSemantic::Translation, wgpu::VertexFormat::Float32x3, 0),
                InstanceAttribute::new(InstanceSemantic::Color, wgpu::VertexFormat::Unorm8x4Bgra, 12),
            ],
        };
    }

    pub fn bgra_instances(app: &mut App) -> &'static [[u8; 3]] {
        camera(app);
        let cube = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(1.0));
        let data = vec![
            BgraInstance {
                position: [-1.0, 0.0, 0.0],
                bgra: [0, 0, 255, 255],
            },
            BgraInstance {
                position: [1.0, 0.0, 0.0],
                bgra: [0, 255, 0, 255],
            },
        ];
        let instances = app
            .resources
            .expect_mut::<InstanceBuffers>()
            .insert(InstanceBuffer::from_vec(data, BufferUsage::Static));
        app.world.spawn((InstancedMesh {
            mesh: cube,
            material: Material::unlit(Vec4::ONE),
            instances,
        },));
        &[[255, 0, 0], [0, 255, 0]]
    }

    /// Viewport cameras over a full-screen one: a picture-in-picture with its own clear color
    /// (cleared with a scissored triangle, not a load op) and an overlay without one. GL's
    /// viewport and scissor origin is the bottom left; wgpu flips them.
    pub fn viewports(app: &mut App) -> &'static [[u8; 3]] {
        camera(app);
        let cube = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(2.0));
        let unlit = |r, g, b| Material::unlit(Vec4::new(r, g, b, 1.0));
        app.world
            .spawn((Transform::from_xyz(-1.5, 0.0, 0.0), Mesh3d(cube), unlit(1.0, 0.0, 0.0)));
        app.world
            .spawn((Transform::from_xyz(50.0, 0.0, 0.0), Mesh3d(cube), unlit(0.0, 1.0, 0.0)));
        app.world
            .spawn((Transform::from_xyz(-50.0, 0.0, 0.0), Mesh3d(cube), unlit(1.0, 0.0, 1.0)));
        // Top right, yellow.
        app.world.spawn((
            Transform::from_xyz(50.0, 0.0, 6.0),
            Camera3d {
                order: 1,
                viewport: Some([0.6, 0.1, 0.35, 0.4]),
                ..Camera3d::default().with_clear_color(Vec4::new(1.0, 1.0, 0.0, 1.0))
            },
        ));
        // Bottom right, no clear color.
        app.world.spawn((
            Transform::from_xyz(-50.0, 0.0, 6.0),
            Camera3d {
                order: 2,
                viewport: Some([0.55, 0.55, 0.4, 0.4]),
                clear_color: None,
                ..Default::default()
            },
        ));
        &[[0, 0, 255], [255, 0, 0], [255, 255, 0], [0, 255, 0], [255, 0, 255]]
    }

    pub const ALL: [(&str, Scene); 7] = [
        ("meshes", meshes),
        ("textured", textured),
        ("render_target", render_target),
        ("array_mips", array_mips),
        ("six_layers", six_layers),
        ("bgra_instances", bgra_instances),
        ("viewports", viewports),
    ];
}

/// Read back a whole 2D texture (single sample, `COPY_SRC`) as linear float RGBA, blocking
/// (tests only). sRGB formats are decoded; `Rgba16Float` is widened.
pub fn read_texture(renderer: &Renderer, texture: &wgpu::Texture) -> Vec<[f32; 4]> {
    let (device, queue) = (renderer.device(), renderer.queue());
    let format = texture.format();
    let texel = format.block_copy_size(None).expect("color format") as usize;
    let (w, h) = (texture.width() as usize, texture.height() as usize);
    let row = (w * texel).next_multiple_of(256);
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("test readback"),
        size: (row * h) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&Default::default());
    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(row as u32),
                rows_per_image: None,
            },
        },
        texture.size(),
    );
    queue.submit([encoder.finish()]);
    buffer.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let data = buffer.slice(..).get_mapped_range().expect("mapped");
    let mut out = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            let p = &data[y * row + x * texel..][..texel];
            out.push(match format {
                wgpu::TextureFormat::Rgba16Float => {
                    [0, 2, 4, 6].map(|i| f16_to_f32(u16::from_le_bytes([p[i], p[i + 1]])))
                }
                f if f.is_srgb() => {
                    let c = |i: usize| srgb_to_linear(p[i] as f32 / 255.0);
                    [c(0), c(1), c(2), p[3] as f32 / 255.0]
                }
                _ => [0, 1, 2, 3].map(|i| p[i] as f32 / 255.0),
            });
        }
    }
    out
}
