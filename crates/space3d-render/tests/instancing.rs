//! Instancing, culling, draw paths and upload accounting.

mod common;
use common::*;
use space3d_core::prelude::*;
use space3d_render::*;

fn cube_grid(app: &mut App, n: i32, spacing: f32) -> MeshHandle {
    let mesh = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(0.5));
    let material = Material::srgb(0.7, 0.7, 0.9);
    for x in 0..n {
        for y in 0..n {
            for z in 0..n {
                let p = Vec3::new(x as f32, y as f32, z as f32) - Vec3::splat((n - 1) as f32 / 2.0);
                app.world.spawn((
                    Transform::from_xyz(p.x * spacing, p.y * spacing, p.z * spacing),
                    Mesh3d(mesh),
                    material.clone(),
                ));
            }
        }
    }
    mesh
}

#[test]
fn thousand_ecs_cubes_are_one_draw_call_on_every_path() {
    let Some(mut app) = headless_app(128, 128) else { return };
    cube_grid(&mut app, 10, 1.0);
    spawn_camera(&mut app, 25.0);
    spawn_sun(&mut app);
    let best = best_path(&app);
    for path in [
        DrawPath::CpuDirect,
        DrawPath::GpuIndirect,
        DrawPath::GpuMultiDraw,
        DrawPath::GpuMultiDrawCount,
    ] {
        if path > best {
            continue;
        }
        set_path(&mut app, path);
        render_to_image(&mut app).expect("capture");
        let s = stats(&app);
        assert_eq!(s.draw_path, path);
        assert_eq!(s.batches, 1, "{path:?}");
        assert_eq!(s.instances, 1000, "{path:?}");
        assert_eq!(s.draw_calls, 1, "{path:?}: 1000 cubes, same mesh+material → 1 draw");
        assert_eq!(s.triangles, 12_000, "{path:?}");
        assert_eq!(s.gpu_culling, path != DrawPath::CpuDirect);
    }
}

#[test]
fn cpu_culling_rejects_offscreen_instances() {
    let Some(mut app) = headless_app(64, 64) else { return };
    let mesh = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(0.5));
    for i in 0..10 {
        let x = i as f32 * 0.6 - 3.0;
        app.world
            .spawn((Transform::from_xyz(x, 0.0, 0.0), Mesh3d(mesh), Material::default()));
        // Behind the camera and far to the side: never visible.
        app.world
            .spawn((Transform::from_xyz(x, 0.0, 50.0), Mesh3d(mesh), Material::default()));
        app.world.spawn((
            Transform::from_xyz(500.0 + x, 0.0, 0.0),
            Mesh3d(mesh),
            Material::default(),
        ));
    }
    spawn_camera(&mut app, 8.0);
    set_path(&mut app, DrawPath::CpuDirect);
    render_to_image(&mut app).expect("capture");
    let s = stats(&app);
    assert_eq!(s.instances, 30);
    assert_eq!(s.culled, 20);
    assert_eq!(s.instances_drawn, 10);
    assert_eq!(s.draw_calls, 1);

    app.resources.expect_mut::<RenderSettings>().frustum_culling = false;
    render_to_image(&mut app).expect("capture");
    assert_eq!(stats(&app).culled, 0);
}

#[test]
fn gpu_culling_matches_visible_only_scene() {
    let Some(mut app) = headless_app(96, 96) else { return };
    let best = best_path(&app);
    if best == DrawPath::CpuDirect {
        eprintln!("skipping: GPU culling unsupported on this adapter");
        return;
    }
    let mesh = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(0.5));
    let visible: Vec<_> = (0..20)
        .map(|i| {
            let p = Vec3::new((i % 5) as f32 - 2.0, (i / 5) as f32 - 1.5, 0.0);
            app.world.spawn((
                Transform::from_xyz(p.x, p.y, p.z),
                Mesh3d(mesh),
                Material::srgb(0.9, 0.5, 0.1),
            ))
        })
        .collect();
    spawn_camera(&mut app, 8.0);
    spawn_sun(&mut app);
    set_path(&mut app, best);
    let only_visible = render_to_image(&mut app).expect("capture");
    for i in 0..200 {
        let p = Vec3::new(1000.0 + i as f32, 0.0, -(i as f32));
        app.world.spawn((
            Transform::from_xyz(p.x, p.y, p.z),
            Mesh3d(mesh),
            Material::srgb(0.9, 0.5, 0.1),
        ));
    }
    let with_hidden = render_to_image(&mut app).expect("capture");
    assert_eq!(stats(&app).instances, 220);
    assert!(
        diff_fraction(&only_visible, &with_hidden, 2) < 0.001,
        "GPU-culled instances must not change the image"
    );
    drop(visible);
}

/// 10k explicit instances: GPU-indirect path and CPU/direct path must render the same image.
#[test]
fn gpu_indirect_matches_cpu_direct_for_10k_instances() {
    let Some(mut app) = headless_app(160, 120) else { return };
    let best = best_path(&app);
    if best == DrawPath::CpuDirect {
        eprintln!("skipping: GPU culling unsupported on this adapter");
        return;
    }
    let mesh = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(0.3));
    let mut buf = InstanceBuffer::with_capacity(10_000, BufferUsage::Static);
    for i in 0..10_000 {
        let (x, y) = ((i % 100) as f32, (i / 100) as f32);
        let p = Vec3::new(x * 0.5 - 25.0, y * 0.5 - 25.0, -((i * 7919) % 13) as f32);
        let rot = Quat::from_rotation_y(i as f32 * 0.37) * Quat::from_rotation_x(i as f32 * 0.11);
        buf.push(InstanceCompact::new(p, rot, 0.8 + (i % 5) as f32 * 0.1));
    }
    let instances = app.resources.expect_mut::<InstanceBuffers>().insert(buf);
    app.world.spawn((InstancedMesh {
        mesh,
        material: Material::srgb(0.3, 0.8, 0.5),
        instances,
    },));
    // Camera sees roughly half the grid so culling is exercised.
    app.world.spawn((
        Transform::from_xyz(10.0, 5.0, 20.0).looking_at(Vec3::new(10.0, 5.0, 0.0), Vec3::Y),
        Camera3d::default().with_clear_color(Vec4::new(0.0, 0.0, 1.0, 1.0)),
    ));
    spawn_sun(&mut app);

    set_path(&mut app, DrawPath::CpuDirect);
    let cpu = render_to_image(&mut app).expect("capture");
    assert_eq!(stats(&app).draw_calls, 1);
    let mut images = vec![];
    for path in [
        DrawPath::GpuIndirect,
        DrawPath::GpuMultiDraw,
        DrawPath::GpuMultiDrawCount,
    ] {
        if path > best {
            continue;
        }
        set_path(&mut app, path);
        let gpu = render_to_image(&mut app).expect("capture");
        let s = stats(&app);
        assert!(
            s.gpu_culling && s.indirect_draw_calls == 1 && s.compute_dispatches >= 1,
            "{path:?}: {s:?}"
        );
        let d = diff_fraction(&cpu, &gpu, 3);
        assert!(
            d < 0.002,
            "{path:?} differs from CPU path in {:.3}% of pixels",
            d * 100.0
        );
        images.push(gpu);
    }
    assert!(
        cpu.pixels.chunks(4).filter(|p| *p != CLEAR).count() > 1000,
        "instances visible"
    );
}

#[test]
fn dirty_range_uploads_only_changed_instances() {
    let Some(mut app) = headless_app(32, 32) else { return };
    let mesh = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(0.1));
    let data: Vec<_> = (0..100_000)
        .map(|i| {
            InstanceCompact::new(
                Vec3::new((i % 1000) as f32 * 0.2, (i / 1000) as f32 * 0.2, 0.0),
                Quat::IDENTITY,
                1.0,
            )
        })
        .collect();
    let h = app
        .resources
        .expect_mut::<InstanceBuffers>()
        .insert(InstanceBuffer::from_vec(data, BufferUsage::Dynamic));
    app.world.spawn((InstancedMesh {
        mesh,
        material: Material::default(),
        instances: h,
    },));
    spawn_camera(&mut app, 50.0);
    render_to_image(&mut app).expect("capture");
    let first = stats(&app);
    assert_eq!(first.instance_bytes_uploaded, 100_000 * 32);

    let bufs = app.resources.expect_mut::<InstanceBuffers>();
    bufs.get_mut::<InstanceCompact>(h)
        .unwrap()
        .set(54_321, InstanceCompact::new(Vec3::ONE, Quat::IDENTITY, 1.0));
    render_to_image(&mut app).expect("capture");
    let s = stats(&app);
    assert_eq!(s.instance_bytes_uploaded, 32, "exactly one 32-byte instance uploaded");
    assert_eq!(s.buffer_reallocations, 0);

    // Many scattered edits coalesce but stay far below a full upload.
    let buf = app
        .resources
        .expect_mut::<InstanceBuffers>()
        .get_mut::<InstanceCompact>(h)
        .unwrap();
    for i in (0..100_000).step_by(10_000) {
        buf.get_mut(i).scale = 2.0;
    }
    render_to_image(&mut app).expect("capture");
    assert_eq!(stats(&app).instance_bytes_uploaded, 10 * 32);
}

#[test]
fn static_buffer_and_static_scene_upload_nothing_after_first_frame() {
    let Some(mut app) = headless_app(32, 32) else { return };
    let mesh = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(0.2));
    let buf = InstanceBuffer::from_vec(
        (0..5000)
            .map(|i| InstanceTransform::from_trs(Vec3::new(i as f32 * 0.01, 0.0, 0.0), Quat::IDENTITY, Vec3::ONE))
            .collect(),
        BufferUsage::Static,
    );
    let h = app.resources.expect_mut::<InstanceBuffers>().insert(buf);
    app.world.spawn((InstancedMesh {
        mesh,
        material: Material::default(),
        instances: h,
    },));
    cube_grid(&mut app, 3, 1.0);
    spawn_camera(&mut app, 10.0);
    spawn_sun(&mut app);
    for frame in 0..4 {
        app.step_ticks(1);
        let s = stats(&app);
        if frame == 0 {
            assert!(s.instance_bytes_uploaded >= 5000 * 64);
        } else {
            assert_eq!(s.instance_bytes_uploaded, 0, "frame {frame}: {s:?}");
            assert_eq!(
                s.bytes_uploaded, 0,
                "frame {frame}: unchanged scene uploads nothing: {s:?}"
            );
            assert_eq!(s.buffer_reallocations, 0, "frame {frame}");
            assert_eq!(
                s.cpu_reallocations, 0,
                "frame {frame}: no scratch growth in steady state"
            );
        }
    }
}

#[test]
fn moving_ecs_entities_upload_only_changed_matrices() {
    let Some(mut app) = headless_app(32, 32) else { return };
    let mesh = cube_grid(&mut app, 10, 1.0);
    let mover = app
        .world
        .spawn((Transform::default(), Mesh3d(mesh), Material::srgb(0.7, 0.7, 0.9)));
    spawn_camera(&mut app, 30.0);
    app.step_ticks(2);
    app.world.get::<&mut Transform>(mover).unwrap().translation.x += 1.0;
    app.step_ticks(1);
    let s = stats(&app);
    assert_eq!(s.instance_bytes_uploaded, 64, "one moved entity = one matrix: {s:?}");
    assert_eq!(s.cpu_reallocations, 0);
}

#[test]
fn custom_packed_instances_with_color_and_layers() {
    let Some(mut app) = headless_app(64, 64) else { return };
    let (mesh, tex) = {
        let a = app.resources.expect_mut::<RenderAssets>();
        let layers = [
            ImageData::filled(4, 4, [255, 0, 0, 255]),
            ImageData::filled(4, 4, [0, 255, 0, 255]),
        ];
        (
            a.add_mesh(MeshData::cube(1.0)),
            a.add_texture(TextureSource::from_layers(&layers).unwrap()),
        )
    };
    let mut buf = InstanceBuffer::new(BufferUsage::Stream);
    buf.push(InstancePacked::new(
        Vec3::new(-0.8, 0.0, 0.0),
        Quat::IDENTITY,
        1.0,
        Vec4::ONE,
        0,
    ));
    buf.push(InstancePacked::new(
        Vec3::new(0.8, 0.0, 0.0),
        Quat::IDENTITY,
        1.0,
        Vec4::ONE,
        1,
    ));
    let instances = app.resources.expect_mut::<InstanceBuffers>().insert(buf);
    app.world.spawn((InstancedMesh {
        mesh,
        material: Material::unlit(Vec4::ONE).with_texture(tex),
        instances,
    },));
    spawn_camera(&mut app, 4.0);
    let img = render_to_image(&mut app).expect("capture");
    assert_eq!(img.get(20, 32), [255, 0, 0, 255], "layer 0 = red");
    assert_eq!(img.get(44, 32), [0, 255, 0, 255], "layer 1 = green");
    assert_eq!(
        stats(&app).instance_bytes_uploaded,
        64,
        "stream buffers upload fully every frame"
    );
}

#[test]
fn replacing_an_instance_buffer_in_a_reused_slot_uses_the_new_data() {
    let Some(mut app) = headless_app(32, 32) else { return };
    let mesh = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(1.0));
    let far = InstanceBuffer::from_vec(
        vec![InstanceCompact::new(Vec3::X * 100.0, Quat::IDENTITY, 1.0)],
        BufferUsage::Static,
    );
    let h1 = app.resources.expect_mut::<InstanceBuffers>().insert(far);
    let e = app.world.spawn((InstancedMesh {
        mesh,
        material: Material::unlit(Vec4::ONE),
        instances: h1,
    },));
    spawn_camera(&mut app, 3.0);
    let img = render_to_image(&mut app).expect("capture");
    assert_eq!(img.get(16, 16), CLEAR);

    let bufs = app.resources.expect_mut::<InstanceBuffers>();
    assert!(bufs.remove(h1));
    let near = InstanceBuffer::from_vec(
        vec![InstanceCompact::new(Vec3::ZERO, Quat::IDENTITY, 1.0)],
        BufferUsage::Static,
    );
    let h2 = bufs.insert(near);
    assert_eq!(h1.index(), h2.index(), "slot reused");
    app.world.get::<&mut InstancedMesh>(e).unwrap().instances = h2;
    let img = render_to_image(&mut app).expect("capture");
    assert_eq!(img.get(16, 16), [255, 255, 255, 255]);
}
