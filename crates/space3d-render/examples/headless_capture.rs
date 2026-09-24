//! Render a lit scene headlessly and save it as a PNG.
//!
//! `cargo run -p space3d-render --example headless_capture -- out.png [cpu|gpu]`

use space3d_core::prelude::*;
use space3d_render::*;

fn main() {
    let mut args = std::env::args().skip(1);
    let out = args.next().unwrap_or_else(|| "capture.png".into());
    let path = match args.next().as_deref() {
        Some("cpu") => Some(DrawPath::CpuDirect),
        Some("gpu") => None,
        _ => None,
    };

    let mut app = App::new();
    app.add_plugin(RenderPlugin::headless(640, 360));
    if !app.resources.contains::<Renderer>() {
        eprintln!("no GPU adapter available");
        return;
    }
    app.resources.expect_mut::<RenderSettings>().draw_path = path;
    let (cube, sphere, plane, checker) = {
        let a = app.resources.expect_mut::<RenderAssets>();
        (
            a.add_mesh(MeshData::cube(1.0)),
            a.add_mesh(MeshData::sphere(0.6, 32, 16)),
            a.add_mesh(MeshData::plane(40.0)),
            a.add_image(ImageData::checker(256, 32, [230, 230, 230, 255], [60, 60, 70, 255])),
        )
    };
    app.world.spawn((
        Transform::from_xyz(0.0, -0.5, 0.0),
        Mesh3d(plane),
        Material::default()
            .with_texture(checker)
            .with_metallic_roughness(0.0, 0.9),
    ));
    for i in 0..7 {
        let x = i as f32 * 1.4 - 4.2;
        let h = i as f32 / 7.0 * std::f32::consts::TAU;
        let color = Vec4::new(
            0.5 + 0.5 * h.cos(),
            0.5 + 0.5 * (h + 2.1).cos(),
            0.5 + 0.5 * (h + 4.2).cos(),
            1.0,
        );
        app.world.spawn((
            Transform::from_xyz(x, 0.0, 0.0).with_rotation(Quat::from_rotation_y(i as f32 * 0.4)),
            Mesh3d(cube),
            Material::color(color).with_metallic_roughness(0.0, 0.4),
        ));
        app.world.spawn((
            Transform::from_xyz(x, 1.3, 0.0),
            Mesh3d(sphere),
            Material::color(color).with_metallic_roughness(1.0, 0.25),
        ));
    }
    // A field of explicit instances in the background.
    let mut buf = InstanceBuffer::new(BufferUsage::Static);
    for i in 0..20_000 {
        let (x, z) = ((i % 200) as f32 * 0.3 - 30.0, -(i / 200) as f32 * 0.3 - 4.0);
        buf.push(InstancePacked::new(
            Vec3::new(x, -0.4 + 0.1 * ((x * 0.7).sin() + (z * 0.5).cos()), z),
            Quat::from_rotation_y(i as f32),
            0.12,
            Vec4::new(0.2, 0.6 + 0.3 * (x * 0.1).sin(), 0.3, 1.0),
            0,
        ));
    }
    let instances = app.resources.expect_mut::<InstanceBuffers>().insert(buf);
    app.world.spawn((InstancedMesh {
        mesh: cube,
        material: Material::default(),
        instances,
    },));
    app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 0.0).looking_at(Vec3::new(-0.4, -1.0, -0.5), Vec3::Y),
        DirectionalLight {
            color: Vec3::new(1.0, 0.95, 0.9),
            intensity: 2.5,
        },
    ));
    app.world.spawn((
        Transform::from_xyz(0.0, 2.5, 2.0),
        PointLight {
            color: Vec3::new(1.0, 0.5, 0.2),
            intensity: 20.0,
            range: 8.0,
        },
    ));
    app.insert_resource(AmbientLight {
        color: Vec3::new(0.6, 0.7, 1.0),
        intensity: 0.15,
    });
    app.world.spawn((
        Transform::from_xyz(0.0, 2.5, 7.5).looking_at(Vec3::new(0.0, 0.3, 0.0), Vec3::Y),
        Camera3d::default().with_clear_color(Vec4::new(0.05, 0.07, 0.12, 1.0)),
    ));

    let img = render_to_image(&mut app).expect("capture");
    let stats = app.resources.expect::<RenderStats>();
    println!("{stats:#?}");
    image::RgbaImage::from_raw(img.width, img.height, img.pixels)
        .expect("size")
        .save(&out)
        .expect("write png");
    println!("wrote {out}");
}
